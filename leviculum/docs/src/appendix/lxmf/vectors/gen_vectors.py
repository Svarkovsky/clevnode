#!/usr/bin/env python3
"""Generate golden test vectors for the LXMF protocol specification.

Every binary claim in the specification is backed by a vector produced here.
The harness imports the vendored Python reference implementations directly
(``reference/LXMF`` and ``reference/Reticulum``) so the vectors are the genuine
output of the reference code at the pinned submodule commits, not a
reimplementation.

Run from the repository root:

    PYTHONPATH=reference/Reticulum:reference/LXMF \
        python3 docs/src/appendix/lxmf/vectors/gen_vectors.py

Output: ``vectors.json`` next to this script. The file is committed and the
specification embeds its hex blocks. Re-running MUST reproduce it byte for
byte; the harness asserts determinism for every stored field before writing.

Vector kinds
------------
frozen     Deterministic bytes. The hex is the proof. Reproducible across runs.
roundtrip  Output depends on ephemeral key material (RNS Destination.encrypt
           uses a fresh ephemeral X25519 key per call). Random ciphertext and
           ciphertext-derived hashes are deliberately omitted from the JSON;
           deterministic lengths, structural assertions and decrypt booleans
           prove the round trip while keeping the fixture reproducible.
"""

import base64
import json
import os
import subprocess

import RNS
from RNS.vendor import umsgpack as msgpack

import LXMF
from LXMF.LXMessage import LXMessage
from LXMF.LXMPeer import LXMPeer
from LXMF.LXMRouter import LXMRouter
from LXMF import (
    compression_support_from_app_data,
    display_name_from_app_data,
    stamp_cost_from_app_data,
    pn_announce_data_is_valid,
    pn_name_from_app_data,
    pn_stamp_cost_from_app_data,
    SF_COMPRESSION,
    PN_META_NAME,
)
from LXMF import LXStamper


EXPECTED_LXMF_VERSION = "1.1.0"
EXPECTED_LXMF_COMMIT = "795fdaa2b0777c13033787d933d1afc94a2377cb"
EXPECTED_RNS_VERSION = "1.3.5"
EXPECTED_RNS_COMMIT = "d5e62d4e15c5fe2e170f7bd9e120551671f21a27"


# --------------------------------------------------------------------------
# Fixed inputs. Nothing here may depend on wall-clock time or system entropy,
# except where a field is wall-clock in the real protocol, in which case it is
# pinned to a constant and called out explicitly.
# --------------------------------------------------------------------------

# 64-byte Identity private material = X25519(32) || Ed25519(32).
SRC_PRV = bytes(range(0, 64))
DST_PRV = bytes(range(64, 128))

# Pinned wall-clock value used for the message timestamp (payload[0]). Real
# senders use time.time(); the spec marks this field as wall-clock.
FIXED_TIMESTAMP = 1700000000.0

# Pinned ticket material. Real issuers use os.urandom(TICKET_LENGTH) and
# time.time()+TICKET_EXPIRY (LXMRouter.py:1095-1097); both are wall-clock or
# entropy in the live protocol and are frozen here so the vector reproduces.
TICKET_SECRET = bytes(range(0x10, 0x20))
INBOUND_TICKET_SECRET = bytes(range(0x20, 0x30))
TICKET_EXPIRES = FIXED_TIMESTAMP + LXMessage.TICKET_EXPIRY

REPO_ROOT = os.path.abspath(
    os.path.join(os.path.dirname(__file__), "..", "..", "..", "..", "..")
)


def _submodule_commit(path):
    try:
        return (
            subprocess.check_output(
                ["git", "-C", os.path.join(REPO_ROOT, path), "rev-parse", "HEAD"],
                stderr=subprocess.DEVNULL,
            )
            .decode()
            .strip()
        )
    except Exception:
        return "unknown"


def assert_reference_lock():
    """Refuse to rewrite the canonical fixture from an unexpected reference."""
    actual_lxmf_commit = _submodule_commit("reference/LXMF")
    actual_rns_commit = _submodule_commit("reference/Reticulum")
    expected = {
        "LXMF version": (LXMF.__version__, EXPECTED_LXMF_VERSION),
        "LXMF commit": (actual_lxmf_commit, EXPECTED_LXMF_COMMIT),
        "RNS version": (RNS.__version__, EXPECTED_RNS_VERSION),
        "RNS commit": (actual_rns_commit, EXPECTED_RNS_COMMIT),
    }
    mismatches = [
        f"{name}: got {actual}, expected {wanted}"
        for name, (actual, wanted) in expected.items()
        if actual != wanted
    ]
    if mismatches:
        raise RuntimeError(
            "reference lock mismatch; review upstream changes before regenerating:\n  "
            + "\n  ".join(mismatches)
        )


def make_identities():
    src_id = RNS.Identity.from_bytes(SRC_PRV)
    dst_id = RNS.Identity.from_bytes(DST_PRV)
    return src_id, dst_id


def delivery_destinations(direction):
    src_id, dst_id = make_identities()
    src = RNS.Destination(src_id, direction, RNS.Destination.SINGLE, "lxmf", "delivery")
    dst = RNS.Destination(dst_id, direction, RNS.Destination.SINGLE, "lxmf", "delivery")
    return src, dst


def build_message(title, content, fields, method):
    """Build and pack a message deterministically at FIXED_TIMESTAMP."""
    src, dst = delivery_destinations(RNS.Destination.OUT)
    m = LXMessage(dst, src, content=content, title=title, fields=fields,
                  desired_method=method)
    m.timestamp = FIXED_TIMESTAMP
    m.pack()
    return m, src, dst


def split_packed(packed):
    """Slice a packed message at the fixed offsets (LXMessage.py:380-383)."""
    return {
        "destination_hash": packed[0:16].hex(),
        "source_hash": packed[16:32].hex(),
        "signature": packed[32:96].hex(),
        "packed_payload": packed[96:].hex(),
    }


VECTORS = []


def add(vec):
    VECTORS.append(vec)
    return vec


# --------------------------------------------------------------------------
# Constants (LXMessage class attributes). Proves the derived sizes.
# --------------------------------------------------------------------------

def collect_constants():
    keys = [
        "DESTINATION_LENGTH", "SIGNATURE_LENGTH", "TICKET_LENGTH",
        "TIMESTAMP_SIZE", "STRUCT_OVERHEAD", "LXMF_OVERHEAD",
        "ENCRYPTED_PACKET_MDU", "ENCRYPTED_PACKET_MAX_CONTENT",
        "LINK_PACKET_MDU", "LINK_PACKET_MAX_CONTENT",
        "PLAIN_PACKET_MDU", "PLAIN_PACKET_MAX_CONTENT",
        "PAPER_MDU", "COST_TICKET",
        "TICKET_EXPIRY", "TICKET_GRACE", "TICKET_RENEW", "TICKET_INTERVAL",
    ]
    out = {}
    for k in keys:
        out[k] = getattr(LXMessage, k)
    stamper = {
        "WORKBLOCK_EXPAND_ROUNDS": LXStamper.WORKBLOCK_EXPAND_ROUNDS,
        "WORKBLOCK_EXPAND_ROUNDS_PN": LXStamper.WORKBLOCK_EXPAND_ROUNDS_PN,
        "WORKBLOCK_EXPAND_ROUNDS_PEERING": LXStamper.WORKBLOCK_EXPAND_ROUNDS_PEERING,
        "STAMP_SIZE": LXStamper.STAMP_SIZE,
    }
    return out, stamper


# --------------------------------------------------------------------------
# Message-format vectors (frozen).
# --------------------------------------------------------------------------

def gen_message_vectors():
    # VEC-MSG-1: minimal opportunistic message, no fields, no stamp.
    m, src, dst = build_message(b"Hi", b"Hello", {}, LXMessage.OPPORTUNISTIC)
    parts = split_packed(m.packed)
    # Re-derive the hashing and signing inputs to prove them.
    payload = [FIXED_TIMESTAMP, b"Hi", b"Hello", {}]
    hashed_part = dst.hash + src.hash + msgpack.packb(payload)
    msg_hash = RNS.Identity.full_hash(hashed_part)
    signed_part = hashed_part + msg_hash
    add({
        "id": "VEC-MSG-1",
        "title": "Minimal opportunistic message (no fields, no stamp)",
        "kind": "frozen",
        "citation": "LXMessage.py:355-388",
        "inputs": {
            "src_identity_prv_hex": SRC_PRV.hex(),
            "dst_identity_prv_hex": DST_PRV.hex(),
            "timestamp": FIXED_TIMESTAMP,
            "title": "Hi", "content": "Hello", "fields": {},
            "desired_method": "OPPORTUNISTIC",
        },
        "packed_hex": m.packed.hex(),
        "packed_len": len(m.packed),
        "parts": parts,
        "payload_msgpack_hex": msgpack.packb(payload).hex(),
        "hashed_part_hex": hashed_part.hex(),
        "message_id_hex": m.message_id.hex(),
        "hash_hex": m.hash.hex(),
        "signed_part_hex": signed_part.hex(),
        "signature_hex": m.signature.hex(),
        "signature_valid": src.identity.validate(m.signature, signed_part),
        "method": m.method,
        "representation": m.representation,
    })

    # VEC-MSG-2: message carrying a fields dict with an integer key.
    fields = {0x0F: 0x02}  # FIELD_RENDERER: RENDERER_MARKDOWN
    m2, src2, dst2 = build_message(b"", b"body text", fields, LXMessage.DIRECT)
    add({
        "id": "VEC-MSG-2",
        "title": "Message with a fields dict (integer field key)",
        "kind": "frozen",
        "citation": "LXMessage.py:355-388,215-219",
        "inputs": {
            "timestamp": FIXED_TIMESTAMP, "title": "", "content": "body text",
            "fields": {"0x0F": "0x02"}, "desired_method": "DIRECT",
        },
        "packed_hex": m2.packed.hex(),
        "packed_len": len(m2.packed),
        "parts": split_packed(m2.packed),
        "payload_msgpack_hex": msgpack.packb([FIXED_TIMESTAMP, b"", b"body text", fields]).hex(),
        "message_id_hex": m2.message_id.hex(),
        "method": m2.method,
        "representation": m2.representation,
    })

    # Negative fixints are valid MessagePack integer map keys. This catches a
    # subtle interop regression where a generic map/string codec rewrites an
    # unknown LXMF field key or rejects it as an unsigned value.
    negative_fields = {-1: b"negative field"}
    m_negative, _, _ = build_message(
        b"negative key", b"body", negative_fields, LXMessage.DIRECT
    )
    negative_payload = msgpack.packb([
        FIXED_TIMESTAMP,
        b"negative key",
        b"body",
        negative_fields,
    ])
    add({
        "id": "VEC-MSG-NEGATIVE-FIELD",
        "title": "Message with a negative-fixint field key",
        "kind": "frozen",
        "citation": "LXMessage.py:355-387",
        "inputs": {
            "timestamp": FIXED_TIMESTAMP,
            "title": "negative key",
            "content": "body",
            "field_key": -1,
            "field_value_hex": b"negative field".hex(),
            "desired_method": "DIRECT",
        },
        "packed_hex": m_negative.packed.hex(),
        "packed_len": len(m_negative.packed),
        "parts": split_packed(m_negative.packed),
        "payload_msgpack_hex": negative_payload.hex(),
        "message_id_hex": m_negative.message_id.hex(),
        "method": m_negative.method,
        "representation": m_negative.representation,
    })

    # VEC-MSG-TICKET: the only message shape where the payload carries five
    # elements. It pins three separable semantics in one set of reference
    # bytes:
    #   * the stamp is appended to the payload AFTER the message ID and the
    #     signature are computed, so neither covers it (LXMessage.py:362-375);
    #   * an outbound ticket produces the 16-byte stamp
    #     truncated_hash(ticket || message_id) (LXMessage.py:299-302);
    #   * an issued ticket travels in FIELD_TICKET as [expires, ticket], the
    #     list generate_ticket() returns (LXMRouter.py:1096-1100, :1772).
    # Ticket stamps are the only deterministic stamp: a proof-of-work stamp is
    # a random search result and cannot be a frozen vector.
    ticket_fields = {0x0C: [TICKET_EXPIRES, INBOUND_TICKET_SECRET]}
    src_t, dst_t = delivery_destinations(RNS.Destination.OUT)
    m_ticket = LXMessage(dst_t, src_t, content=b"ticketed", title=b"T",
                         fields=ticket_fields, desired_method=LXMessage.DIRECT)
    m_ticket.timestamp = FIXED_TIMESTAMP
    m_ticket.outbound_ticket = TICKET_SECRET
    m_ticket.defer_stamp = False
    m_ticket.pack()
    unstamped_payload = [FIXED_TIMESTAMP, b"T", b"ticketed", ticket_fields]
    ticket_hashed_part = (
        dst_t.hash + src_t.hash + msgpack.packb(unstamped_payload)
    )
    ticket_msg_hash = RNS.Identity.full_hash(ticket_hashed_part)
    ticket_signed_part = ticket_hashed_part + ticket_msg_hash
    add({
        "id": "VEC-MSG-TICKET",
        "title": "Ticket-stamped message (five-element payload)",
        "kind": "frozen",
        "citation": "LXMessage.py:355-388,293-302; LXMRouter.py:1096-1100,1770-1772",
        "inputs": {
            "timestamp": FIXED_TIMESTAMP,
            "title": "T",
            "content": "ticketed",
            "outbound_ticket_hex": TICKET_SECRET.hex(),
            "field_ticket_expires": TICKET_EXPIRES,
            "field_ticket_secret_hex": INBOUND_TICKET_SECRET.hex(),
            "desired_method": "DIRECT",
        },
        "packed_hex": m_ticket.packed.hex(),
        "packed_len": len(m_ticket.packed),
        "parts": split_packed(m_ticket.packed),
        "unstamped_payload_msgpack_hex": msgpack.packb(unstamped_payload).hex(),
        "stamped_payload_msgpack_hex": msgpack.packb(
            unstamped_payload + [m_ticket.stamp]
        ).hex(),
        "hashed_part_hex": ticket_hashed_part.hex(),
        "message_id_hex": m_ticket.message_id.hex(),
        "signed_part_hex": ticket_signed_part.hex(),
        "signature_hex": m_ticket.signature.hex(),
        "signature_valid": src_t.identity.validate(
            m_ticket.signature, ticket_signed_part
        ),
        "stamp_hex": m_ticket.stamp.hex(),
        "stamp_len": len(m_ticket.stamp),
        "stamp_value": m_ticket.stamp_value,
        "ticket_field_msgpack_hex": msgpack.packb(
            [TICKET_EXPIRES, INBOUND_TICKET_SECRET]
        ).hex(),
        "stamp_excluded_from_message_id": (
            m_ticket.message_id == ticket_msg_hash
        ),
        "note": (
            "payload[4] is the stamp; message_id and signature are computed "
            "over the four-element payload only."
        ),
    })

    # VEC-MSG-3: unpack round trip proves the offsets + verification path.
    # Make the source identity recallable so unpack runs the full signature
    # validation branch (LXMessage.py:765-797). In the live protocol the
    # source identity is learned from its announce; here we inject it.
    RNS.Identity.remember(None, src.hash, src.identity.get_public_key())
    unpacked = LXMessage.unpack_from_bytes(m.packed)
    add({
        "id": "VEC-MSG-3",
        "title": "Unpack + signature verification round trip of VEC-MSG-1",
        "kind": "frozen",
        "citation": "LXMessage.py:747-822",
        "source_vector": "VEC-MSG-1",
        "recovered_message_id_hex": unpacked.hash.hex(),
        "recovered_title": unpacked.title_as_string(),
        "recovered_content": unpacked.content_as_string(),
        "signature_validated": bool(unpacked.signature_validated),
        "matches_source": unpacked.hash == m.hash,
    })

    gen_foreign_writer_vectors(src, dst)


# --------------------------------------------------------------------------
# Foreign-writer vectors (frozen).
#
# LXMF always writes time.time(), so payload[0] from a Python peer is always
# float64. These vectors cover the other case: a third-party writer that packs
# the same value with a different MessagePack type. The reference reads
# payload[0] with no type check (LXMessage.py:766) and, when the payload has no
# stamp, hashes the received bytes verbatim (packed_payload, :753,762), so it
# accepts them. The verdict recorded in each vector is the reference's own.
# --------------------------------------------------------------------------

FOREIGN_TIMESTAMP_FORMS = [
    ("VEC-MSG-FOREIGN-UINT32", "uint32", bytes([0xCE, 0x65, 0x53, 0xF1, 0x00])),
    ("VEC-MSG-FOREIGN-FLOAT32", "float32", bytes([0xCA, 0x3F, 0xC0, 0x00, 0x00])),
    ("VEC-MSG-FOREIGN-NEGATIVE-FIXINT", "negative fixint", bytes([0xFF])),
]


def gen_foreign_writer_vectors(src, dst):
    RNS.Identity.remember(None, src.hash, src.identity.get_public_key())
    for vector_id, form, timestamp_bytes in FOREIGN_TIMESTAMP_FORMS:
        # payload[0] spliced in verbatim; the rest is the canonical form a
        # Python writer produces, so the timestamp type is the only variable.
        packed_payload = (
            b"\x94"
            + timestamp_bytes
            + msgpack.packb(b"Hi")
            + msgpack.packb(b"Hello")
            + msgpack.packb({})
        )
        # A writer signs what it packed. This is LXMessage.pack() with the
        # hand-built payload substituted (LXMessage.py:362-375).
        hashed_part = dst.hash + src.hash + packed_payload
        message_hash = RNS.Identity.full_hash(hashed_part)
        signed_part = hashed_part + message_hash
        signature = src.identity.sign(signed_part)
        lxmf_bytes = dst.hash + src.hash + signature + packed_payload

        recovered = LXMessage.unpack_from_bytes(lxmf_bytes)
        add({
            "id": vector_id,
            "title": f"Foreign writer: payload[0] as MessagePack {form}",
            "kind": "frozen",
            "citation": "LXMessage.py:747-766",
            "inputs": {
                "timestamp_msgpack_hex": timestamp_bytes.hex(),
                "timestamp_form": form,
                "title": "Hi",
                "content": "Hello",
                "fields": {},
            },
            "packed_hex": lxmf_bytes.hex(),
            "packed_len": len(lxmf_bytes),
            "parts": split_packed(lxmf_bytes),
            "message_id_hex": message_hash.hex(),
            "reference_timestamp_repr": repr(recovered.timestamp),
            "reference_timestamp_type": type(recovered.timestamp).__name__,
            "reference_signature_validated": bool(recovered.signature_validated),
            "reference_message_id_matches": recovered.hash == message_hash,
            "note": (
                "The reference accepts the message and validates the signature "
                "against the bytes the writer packed, because an unstamped "
                "payload is hashed as received."
            ),
        })


# --------------------------------------------------------------------------
# Delivery-method vectors.
# --------------------------------------------------------------------------

def gen_delivery_vectors():
    # Opportunistic on-air form: packed with the leading dest hash removed
    # (LXMessage.__as_packet, LXMessage.py:623-635).
    m, src, dst = build_message(b"Hi", b"Hello", {}, LXMessage.OPPORTUNISTIC)
    add({
        "id": "VEC-DLV-OPP",
        "title": "Opportunistic on-air payload (leading destination hash omitted)",
        "kind": "frozen",
        "citation": "LXMessage.py:626-638",
        "full_packed_hex": m.packed.hex(),
        "on_air_hex": m.packed[16:].hex(),
        "note": "Destination is inferred from the RNS packet header.",
    })

    # Direct: full packed bytes are sent (over a Link), as VEC-MSG-1.
    add({
        "id": "VEC-DLV-DIRECT",
        "title": "Direct delivery sends the full packed message",
        "kind": "frozen",
        "citation": "LXMessage.py:417-424,635-636",
        "on_air_hex": m.packed.hex(),
        "note": "Sent as a single Packet over a Link when content fits "
                "LINK_PACKET_MAX_CONTENT, else as a Resource.",
    })

    # Propagated: encrypted envelope. Non-deterministic ciphertext -> roundtrip.
    mp, srcp, dstp = build_message(b"Hi", b"Hello", {}, LXMessage.PROPAGATED)
    # mp.packed is the inner message; reconstruct the envelope structure.
    inner = mp.packed
    pn_encrypted = dstp.encrypt(inner[16:])
    lxmf_data = inner[:16] + pn_encrypted
    transient_id = RNS.Identity.full_hash(lxmf_data)
    recovered = dstp.decrypt(pn_encrypted)
    add({
        "id": "VEC-PROP-ENVELOPE",
        "title": "Propagation transfer envelope (encrypted, round-trip proof)",
        "kind": "roundtrip",
        "citation": "LXMessage.py:426-436",
        "inner_packed_hex": inner.hex(),
        "dest_hash_prefix_hex": inner[:16].hex(),
        "pn_encrypted_len": len(pn_encrypted),
        "lxmf_data_structure": "destination_hash(16) || destination.encrypt(packed[16:])",
        "transient_id_len": len(transient_id),
        "transient_id_is_full_hash": transient_id
                                     == RNS.Identity.full_hash(lxmf_data),
        "transient_id_note": "transient_id = full_hash(lxmf_data); random "
                             "ciphertext-derived bytes are intentionally omitted.",
        "envelope_structure": "msgpack([wall_clock_timestamp, [lxmf_data, ...]])",
        "decrypt_recovers_inner_tail": recovered == inner[16:],
    })

    # Paper: lxm:// URI. Encrypted -> roundtrip + structure.
    mpaper, srcpr, dstpr = build_message(b"Hi", b"Hello", {}, LXMessage.PAPER)
    inner_paper = mpaper.packed
    uri = mpaper.as_uri()
    encoded_body = uri.split("://", 1)[1]
    padded_body = encoded_body + "=" * ((4 - len(encoded_body) % 4) % 4)
    paper_bytes = base64.urlsafe_b64decode(padded_body)
    recovered_paper_tail = dstpr.decrypt(paper_bytes[16:])
    add({
        "id": "VEC-PAPER-URI",
        "title": "Paper message lxm:// URI (encrypted, round-trip proof)",
        "kind": "roundtrip",
        "citation": "LXMessage.py:446-451,698-713",
        "uri_scheme": LXMessage.URI_SCHEMA,
        "uri_prefix": LXMessage.URI_SCHEMA + "://",
        "uri_length": len(uri),
        "padding_stripped": not uri.endswith("="),
        "destination_hash_matches": paper_bytes[:16] == inner_paper[:16],
        "decrypt_recovers_inner_tail": recovered_paper_tail == inner_paper[16:],
        "structure": "lxm://base64url(destination_hash(16) || "
                     "destination.encrypt(packed[16:])), '=' padding stripped",
        "note": "The random base64url body is intentionally omitted.",
    })


# --------------------------------------------------------------------------
# Stamp / proof-of-work vectors (frozen, low cost for tractability).
# --------------------------------------------------------------------------

def gen_stamp_vectors():
    # Use a fixed 32-byte material (stand-in for a message_id) and a tiny
    # expand_rounds so the vector is cheap to reproduce. The spec documents
    # that real delivery stamps use WORKBLOCK_EXPAND_ROUNDS (3000); this
    # vector pins the ALGORITHM, with the round count as an explicit input.
    material = RNS.Identity.full_hash(b"lxmf-spec-stamp-material")
    rounds = 4
    target_cost = 8
    workblock = LXStamper.stamp_workblock(material, expand_rounds=rounds)

    # Deterministic search: stamp = full_hash(material || counter) until valid.
    stamp = None
    counter = 0
    while True:
        cand = RNS.Identity.full_hash(material + counter.to_bytes(8, "big"))
        if LXStamper.stamp_valid(cand, target_cost, workblock):
            stamp = cand
            break
        counter += 1
    value = LXStamper.stamp_value(workblock, stamp)
    digest = RNS.Identity.full_hash(workblock + stamp)
    add({
        "id": "VEC-STAMP-1",
        "title": "Stamp workblock, validity, and value (cost=8, expand_rounds=4)",
        "kind": "frozen",
        "citation": "LXStamper.py:49-77",
        "material_hex": material.hex(),
        "expand_rounds": rounds,
        "target_cost": target_cost,
        "workblock_len": len(workblock),
        "workblock_sha256_hex": RNS.Identity.full_hash(workblock).hex(),
        "stamp_search": "stamp = full_hash(material || counter_be8), counter++ until valid",
        "winning_counter": counter,
        "stamp_hex": stamp.hex(),
        "digest_hex": digest.hex(),
        "target_hex": (0b1 << (256 - target_cost)).to_bytes(32, "big").hex(),
        "valid": LXStamper.stamp_valid(stamp, target_cost, workblock),
        "stamp_value": value,
    })

    # Pin the separate propagation-node expansion count. A second low-cost
    # deterministic search keeps the fixture quick while still exercising the
    # complete 1000-round workblock used for real origin uploads.
    pn_material = bytes(range(32))
    pn_rounds = LXStamper.WORKBLOCK_EXPAND_ROUNDS_PN
    pn_cost = 8
    pn_workblock = LXStamper.stamp_workblock(
        pn_material, expand_rounds=pn_rounds
    )
    pn_counter = 0
    while True:
        pn_stamp = pn_counter.to_bytes(32, "big")
        if LXStamper.stamp_valid(pn_stamp, pn_cost, pn_workblock):
            break
        pn_counter += 1
    add({
        "id": "VEC-STAMP-PN",
        "title": "Propagation-node stamp (cost=8, expand_rounds=1000)",
        "kind": "frozen",
        "citation": "LXStamper.py:13,53-63,122-155",
        "material_hex": pn_material.hex(),
        "expand_rounds": pn_rounds,
        "target_cost": pn_cost,
        "workblock_len": len(pn_workblock),
        "workblock_sha256_hex": RNS.Identity.full_hash(pn_workblock).hex(),
        "stamp_search": "stamp = counter_be32, counter++ until valid",
        "winning_counter": pn_counter,
        "stamp_hex": pn_stamp.hex(),
        "digest_hex": RNS.Identity.full_hash(pn_workblock + pn_stamp).hex(),
        "target_hex": (0b1 << (256 - pn_cost)).to_bytes(32, "big").hex(),
        "valid": LXStamper.stamp_valid(pn_stamp, pn_cost, pn_workblock),
        "stamp_value": LXStamper.stamp_value(pn_workblock, pn_stamp),
    })


# --------------------------------------------------------------------------
# Propagation mailbox client vectors (frozen).
# --------------------------------------------------------------------------

def _transient_id(start):
    return bytes((start + offset) & 0xff for offset in range(32))


def gen_propagation_client_vectors():
    """Pin every client-side ``/get`` request and response wire direction."""
    first_id = _transient_id(0)
    wanted_id = _transient_id(32)
    held_id = _transient_id(64)

    list_request = [None, None]
    add({
        "id": "VEC-PROP-GET-LIST",
        "title": "Propagation mailbox list request",
        "kind": "frozen",
        "citation": "LXMRouter.py:492-501; LXMPeer.py:15",
        "path": LXMPeer.MESSAGE_GET_PATH,
        "value": [None, None],
        "request_hex": msgpack.packb(list_request).hex(),
    })

    download_request = [[wanted_id], [held_id], 1000]
    add({
        "id": "VEC-PROP-GET-DOWNLOAD",
        "title": "Propagation mailbox download request",
        "kind": "frozen",
        "citation": "LXMRouter.py:1521-1539; LXMPeer.py:15",
        "path": LXMPeer.MESSAGE_GET_PATH,
        "wants_hex": [wanted_id.hex()],
        "haves_hex": [held_id.hex()],
        "transfer_limit_kb": 1000,
        "request_hex": msgpack.packb(download_request).hex(),
    })

    acknowledge_request = [None, [first_id]]
    add({
        "id": "VEC-PROP-GET-ACK",
        "title": "Propagation mailbox acknowledgement and purge request",
        "kind": "frozen",
        "citation": "LXMRouter.py:1569-1581; LXMPeer.py:15",
        "path": LXMPeer.MESSAGE_GET_PATH,
        "haves_hex": [first_id.hex()],
        "request_hex": msgpack.packb(acknowledge_request).hex(),
    })

    list_response = [wanted_id, held_id]
    add({
        "id": "VEC-PROP-LIST-RESPONSE",
        "title": "Propagation mailbox transient-ID list response",
        "kind": "frozen",
        "citation": "LXMRouter.py:1426-1448,1506-1549",
        "transient_ids_hex": [wanted_id.hex(), held_id.hex()],
        "response_hex": msgpack.packb(list_response).hex(),
    })

    get_response = [b"one", b"two"]
    add({
        "id": "VEC-PROP-GET-RESPONSE",
        "title": "Propagation mailbox downloaded-message response",
        "kind": "frozen",
        "citation": "LXMRouter.py:1450-1500,1551-1588",
        "messages_hex": [value.hex() for value in get_response],
        "response_hex": msgpack.packb(get_response).hex(),
        "no_identity_error_hex": msgpack.packb(LXMPeer.ERROR_NO_IDENTITY).hex(),
        "no_access_error_hex": msgpack.packb(LXMPeer.ERROR_NO_ACCESS).hex(),
    })


# --------------------------------------------------------------------------
# Announce application-data vectors (frozen), proven via genuine decoders.
# --------------------------------------------------------------------------

def gen_announce_vectors():
    # Current delivery announce app_data includes the supported-functionality
    # list added by LXMF 1.0.1 (LXMRouter.get_announce_app_data).
    display_name = "Alice".encode("utf-8")
    stamp_cost = 8
    supported_functionality = [SF_COMPRESSION]
    app_data = msgpack.packb([
        display_name,
        stamp_cost,
        supported_functionality,
    ])
    add({
        "id": "VEC-ANN-DELIVERY",
        "title": "Delivery announce app_data",
        "kind": "frozen",
        "citation": "LXMRouter.py:985-1001; LXMF.py:151-200",
        "structure": "msgpack([display_name_utf8_or_None, stamp_cost_or_None, "
                     "supported_functionality])",
        "app_data_hex": app_data.hex(),
        "first_byte_hex": "%02x" % app_data[0],
        "first_byte_note": "0x93 = msgpack fixarray(3); decoders sniff 0x90-0x9f or 0xdc.",
        "decoded_display_name": display_name_from_app_data(app_data),
        "decoded_stamp_cost": stamp_cost_from_app_data(app_data),
        "decoded_compression_supported": bool(
            compression_support_from_app_data(app_data)
        ),
    })

    # The reference's advertised-stamp-cost window, taken from the reference's
    # own emitter and its own decoder rather than rebuilt here.
    #
    # `get_announce_app_data` (LXMRouter.py:1042-1045) writes the cost only when
    # `0 < cost < 255` and writes None otherwise, so 0 and 255 are indis-
    # tinguishable on the wire from "no cost". `set_inbound_stamp_cost`
    # (LXMRouter.py:378-393) applies the same window one layer earlier and makes
    # the refusal visible: `< 1` is accepted and stored as None (returns True),
    # `>= 255` is refused outright (returns False, previous value untouched).
    #
    # `LXMRouter.__new__` skips `__init__`; both methods read nothing but
    # `self.delivery_destinations`, so this exercises the genuine reference code
    # without a live RNS instance.
    router = LXMRouter.__new__(LXMRouter)
    src_id, _ = make_identities()
    window_destination = RNS.Destination(
        src_id, RNS.Destination.OUT, RNS.Destination.SINGLE, "lxmf", "delivery"
    )
    window_destination.display_name = "Alice"
    router.delivery_destinations = {window_destination.hash: window_destination}

    window = {
        "id": "VEC-ANN-STAMP-COST-WINDOW",
        "title": "Advertised stamp cost is only emitted for 0 < cost < 255",
        "kind": "frozen",
        "citation": "LXMRouter.py:1042-1045 (emit); LXMRouter.py:378-393 (setter)",
        "structure": "emit_<cost>_hex = get_announce_app_data() with that cost "
                     "assigned; peer_reads_<cost> = stamp_cost_from_app_data() "
                     "of those bytes; setter_accepts_<cost> = "
                     "set_inbound_stamp_cost() return value",
        "display_name_hex": window_destination.display_name.encode("utf-8").hex(),
    }
    for cost in [None, 0, 1, 8, 254, 255]:
        key = "none" if cost is None else str(cost)
        # Assign the attribute directly, the way a caller storing an
        # unvalidated configuration value would, so the emitter's own window is
        # what is being measured.
        window_destination.stamp_cost = cost
        emitted = router.get_announce_app_data(window_destination.hash)
        window[f"emit_{key}_hex"] = emitted.hex()
        window[f"peer_reads_{key}"] = stamp_cost_from_app_data(emitted)
        # Then the setter, from a known-clear state, for its return value.
        window_destination.stamp_cost = None
        window[f"setter_accepts_{key}"] = bool(
            router.set_inbound_stamp_cost(window_destination.hash, cost)
        )
        window[f"setter_stores_{key}"] = window_destination.stamp_cost
    add(window)

    # Propagation announce app_data (7-element list) per
    # LXMRouter.get_propagation_node_app_data (LXMRouter.py:306-318).
    FIXED_TIMEBASE = 1700000000  # int(time.time()) in the real protocol.
    metadata = {PN_META_NAME: "Node".encode("utf-8")}
    stamp_costs = [16, 3, 18]  # [prop_cost, prop_flex, peering_cost]
    announce_data = [
        False,            # 0: legacy flag
        FIXED_TIMEBASE,   # 1: node timebase (wall-clock int)
        True,             # 2: propagation enabled
        256,              # 3: per-transfer limit (KB)
        256 * 40,         # 4: per-sync limit (KB)
        stamp_costs,      # 5: [prop_cost, prop_flex, peering_cost]
        metadata,         # 6: metadata map
    ]
    pn_app_data = msgpack.packb(announce_data)
    add({
        "id": "VEC-ANN-PROPAGATION",
        "title": "Propagation node announce app_data (7-element list)",
        "kind": "frozen",
        "citation": "LXMRouter.py:306-318; LXMF.py:202-250",
        "structure": "msgpack([legacy, timebase, enabled, xfer_limit_kb, "
                     "sync_limit_kb, [prop_cost, prop_flex, peering_cost], metadata])",
        "fixed_timebase": FIXED_TIMEBASE,
        "timebase_note": "Field 1 is int(time.time()) in the real protocol.",
        "app_data_hex": pn_app_data.hex(),
        "valid": bool(pn_announce_data_is_valid(pn_app_data)),
        "decoded_pn_name": pn_name_from_app_data(pn_app_data),
        "decoded_pn_stamp_cost": pn_stamp_cost_from_app_data(pn_app_data),
    })


# --------------------------------------------------------------------------
# Determinism self-check for frozen vectors.
# --------------------------------------------------------------------------

def assert_determinism():
    """Rebuild every vector once more and assert the fixture is byte-stable."""
    snapshot = {v["id"]: json.dumps(v, sort_keys=True) for v in VECTORS}
    VECTORS.clear()
    gen_message_vectors()
    gen_delivery_vectors()
    gen_stamp_vectors()
    gen_propagation_client_vectors()
    gen_announce_vectors()
    for v in VECTORS:
        again = json.dumps(v, sort_keys=True)
        if snapshot[v["id"]] != again:
            raise AssertionError(
                f"Non-deterministic vector {v['id']}: output changed "
                f"between runs."
            )


def main():
    assert_reference_lock()
    constants, stamper = collect_constants()
    gen_message_vectors()
    gen_delivery_vectors()
    gen_stamp_vectors()
    gen_propagation_client_vectors()
    gen_announce_vectors()
    assert_determinism()

    doc = {
        "_comment": "Canonical golden vectors for the LXMF protocol specification. "
                    "Generated by gen_vectors.py from the vendored reference. "
                    "Do not edit by hand; re-run the harness.",
        "meta": {
            "lxmf_version": LXMF.__version__,
            "rns_version": RNS.__version__,
            "lxmf_commit": _submodule_commit("reference/LXMF"),
            "reticulum_commit": _submodule_commit("reference/Reticulum"),
            "fixed_timestamp": FIXED_TIMESTAMP,
            "src_identity_prv_hex": SRC_PRV.hex(),
            "dst_identity_prv_hex": DST_PRV.hex(),
        },
        "constants": constants,
        "stamper_constants": stamper,
        "vectors": VECTORS,
    }

    out_path = os.path.join(os.path.dirname(__file__), "vectors.json")
    with open(out_path, "w") as f:
        json.dump(doc, f, indent=2, sort_keys=False)
        f.write("\n")

    print(f"Wrote {len(VECTORS)} vectors to {out_path}")
    print(f"LXMF {LXMF.__version__} @ {doc['meta']['lxmf_commit'][:10]}, "
          f"RNS {RNS.__version__} @ {doc['meta']['reticulum_commit'][:10]}")
    for v in VECTORS:
        print(f"  {v['id']:20s} [{v['kind']:9s}] {v['title']}")


if __name__ == "__main__":
    main()
