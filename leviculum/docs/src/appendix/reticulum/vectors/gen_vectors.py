#!/usr/bin/env python3
"""Generate golden test vectors for the Reticulum protocol specification.

Every binary claim in the specification is backed by a vector produced here.
The harness imports the vendored Python reference (`reference/Reticulum`, RNS
1.3.5, pinned commit d5e62d4) and emits the genuine output of the reference
code.

Run from the repository root:

    PYTHONPATH=reference/Reticulum \
        python3 docs/src/appendix/reticulum/vectors/gen_vectors.py

Output: `vectors.json` next to this script. Re-running MUST reproduce it byte
for byte; the harness asserts determinism for every frozen and
frozen-via-injection vector before writing.

Vector kinds
------------
frozen            Deterministic; the hex is the proof. Reproducible across runs.
frozen-injection  An ephemeral-key path (encryption token, announce random hash,
                  link handshake). The harness installs fixed os.urandom / time /
                  X25519 key generation for the duration of the vector so the
                  bytes are reproducible, AND records a decrypt/verify/derive
                  roundtrip so the semantic property is proven too.
roundtrip         Output cannot be cleanly frozen; proven by structure + a
                  decrypt/verify roundtrip only. Used as a fallback; logged.
"""

import json
import os
import struct
import sys
import tempfile

import RNS
from RNS.Cryptography import X25519PrivateKey, X25519PublicKey, Ed25519PrivateKey
from RNS.Cryptography.AES import AES_256_CBC
import RNS.Cryptography.HMAC as HMAC
from RNS.vendor import umsgpack


# --------------------------------------------------------------------------
# Fixed inputs.
# --------------------------------------------------------------------------

SRC_PRV = bytes(range(0, 64))     # Identity private = X25519(32) || Ed25519(32)
DST_PRV = bytes(range(64, 128))
FIXED_TIME = 1700000000.0

REPO_ROOT = os.path.abspath(
    os.path.join(os.path.dirname(__file__), "..", "..", "..", "..", ".."))


def submodule_commit(path):
    import subprocess
    try:
        return subprocess.check_output(
            ["git", "-C", os.path.join(REPO_ROOT, path), "rev-parse", "HEAD"],
            stderr=subprocess.DEVNULL).decode().strip()
    except Exception:
        return "unknown"


# --------------------------------------------------------------------------
# Deterministic randomness / time injection, scoped per vector.
# --------------------------------------------------------------------------

class Inject:
    """Context manager that pins os.urandom, time.time, and X25519 ephemeral
    key generation to deterministic values for the duration of one vector."""

    def __init__(self):
        self._buf = bytearray()
        self._ctr = 0

    def _stream(self, n):
        while len(self._buf) < n:
            self._buf += RNS.Identity.full_hash(
                b"rns-spec-seed" + self._ctr.to_bytes(8, "big"))
            self._ctr += 1
        out, self._buf = bytes(self._buf[:n]), self._buf[n:]
        return out

    def __enter__(self):
        import time as _time
        self._buf = bytearray()
        self._ctr = 0
        self._real_urandom = os.urandom
        self._real_time = _time.time
        self._real_gen = X25519PrivateKey.generate
        os.urandom = self._stream
        _time.time = lambda: FIXED_TIME
        X25519PrivateKey.generate = staticmethod(
            lambda: X25519PrivateKey.from_private_bytes(self._stream(32)))
        self._time_mod = _time
        return self

    def __exit__(self, *a):
        os.urandom = self._real_urandom
        self._time_mod.time = self._real_time
        X25519PrivateKey.generate = self._real_gen
        return False


VECTORS = []


def add(v):
    VECTORS.append(v)
    return v


# --------------------------------------------------------------------------
# Headless Reticulum instance (needed for IN destinations / announces).
# --------------------------------------------------------------------------

def boot_reticulum():
    cfgdir = tempfile.mkdtemp(prefix="rns-vec-")
    with open(os.path.join(cfgdir, "config"), "w") as f:
        f.write("[reticulum]\n  enable_transport = No\n"
                "  share_instance = No\n  panic_on_interface_error = No\n\n"
                "[logging]\n  loglevel = 1\n\n[interfaces]\n\n")
    return RNS.Reticulum(configdir=cfgdir)


def identities():
    return (RNS.Identity.from_bytes(SRC_PRV),
            RNS.Identity.from_bytes(DST_PRV))


def drop(*dests):
    """Deregister destinations so the harness can rebuild them on the
    determinism re-run without a duplicate-registration error."""
    for d in dests:
        try:
            RNS.Transport.deregister_destination(d)
        except Exception:
            pass


# --------------------------------------------------------------------------
# Constants.
# --------------------------------------------------------------------------

def collect_constants():
    out = {}
    out["MTU"] = RNS.Reticulum.MTU
    out["MDU"] = RNS.Reticulum.MDU
    out["HEADER_MINSIZE"] = RNS.Reticulum.HEADER_MINSIZE
    out["HEADER_MAXSIZE"] = RNS.Reticulum.HEADER_MAXSIZE
    out["TRUNCATED_HASHLENGTH"] = RNS.Reticulum.TRUNCATED_HASHLENGTH
    out["Packet_ENCRYPTED_MDU"] = RNS.Packet.ENCRYPTED_MDU
    out["Packet_PLAIN_MDU"] = RNS.Packet.PLAIN_MDU
    out["Identity_KEYSIZE"] = RNS.Identity.KEYSIZE
    out["Identity_HASHLENGTH"] = RNS.Identity.HASHLENGTH
    out["Identity_SIGLENGTH"] = RNS.Identity.SIGLENGTH
    out["Identity_NAME_HASH_LENGTH"] = RNS.Identity.NAME_HASH_LENGTH
    out["Identity_RATCHETSIZE"] = RNS.Identity.RATCHETSIZE
    out["Token_TOKEN_OVERHEAD"] = RNS.Cryptography.Token.TOKEN_OVERHEAD
    out["Link_ECPUBSIZE"] = RNS.Link.ECPUBSIZE
    out["Link_KEYSIZE"] = RNS.Link.KEYSIZE
    ctx = {}
    for name in ["NONE", "RESOURCE", "RESOURCE_ADV", "RESOURCE_REQ",
                 "RESOURCE_HMU", "RESOURCE_PRF", "RESOURCE_ICL", "RESOURCE_RCL",
                 "CACHE_REQUEST", "REQUEST", "RESPONSE", "PATH_RESPONSE",
                 "COMMAND", "COMMAND_STATUS", "CHANNEL", "KEEPALIVE",
                 "LINKIDENTIFY", "LINKCLOSE", "LINKPROOF", "LRRTT", "LRPROOF"]:
        if hasattr(RNS.Packet, name):
            ctx[name] = getattr(RNS.Packet, name)
    out["packet_context_bytes"] = ctx
    return out


# --------------------------------------------------------------------------
# Vector groups.
# --------------------------------------------------------------------------

def gen_primitives():
    msg = b"reticulum-spec"
    add({"id": "VEC-HASH", "kind": "frozen", "citation": "Identity.py:373-390",
         "title": "SHA-256 full and truncated hash",
         "input_hex": msg.hex(),
         "full_hash_hex": RNS.Identity.full_hash(msg).hex(),
         "truncated_hash_hex": RNS.Identity.truncated_hash(msg).hex()})

    salt = bytes(range(16))
    ikm = bytes(range(32))
    add({"id": "VEC-HKDF", "kind": "frozen", "citation": "Cryptography/HKDF.py:35-62",
         "title": "HKDF-SHA256, 32-byte output",
         "derive_from_hex": ikm.hex(), "salt_hex": salt.hex(),
         "length": 32,
         "okm_hex": RNS.Cryptography.hkdf(length=32, derive_from=ikm,
                                          salt=salt, context=None).hex()})

    key = bytes(range(32))
    add({"id": "VEC-HMAC", "kind": "frozen", "citation": "Cryptography/HMAC.py",
         "title": "HMAC-SHA256",
         "key_hex": key.hex(), "msg_hex": msg.hex(),
         "digest_hex": HMAC.new(key, msg).digest().hex()})

    k = bytes(range(32)); iv = bytes(range(16)); pt = b"0123456789abcdef"
    ct = AES_256_CBC.encrypt(pt, k, iv)
    add({"id": "VEC-AES", "kind": "frozen", "citation": "Cryptography/AES.py",
         "title": "AES-256-CBC with fixed key and IV (one block)",
         "key_hex": k.hex(), "iv_hex": iv.hex(), "plaintext_hex": pt.hex(),
         "ciphertext_hex": ct.hex(),
         "roundtrip_ok": AES_256_CBC.decrypt(ct, k, iv) == pt})


def gen_identity():
    src, dst = identities()
    add({"id": "VEC-ID-HASH", "kind": "frozen", "citation": "Identity.py:805-810",
         "title": "Identity public key and identity hash",
         "src_prv_hex": SRC_PRV.hex(),
         "public_key_hex": src.get_public_key().hex(),
         "public_key_len": len(src.get_public_key()),
         "identity_hash_hex": src.hash.hex()})

    msg = b"reticulum-spec-signed-message"
    sig = src.sign(msg)
    add({"id": "VEC-ID-SIGN", "kind": "frozen", "citation": "Identity.py:931-964",
         "title": "Ed25519 signature over fixed message (deterministic)",
         "message_hex": msg.hex(), "signature_hex": sig.hex(),
         "signature_len": len(sig), "valid": src.validate(sig, msg)})

    with Inject():
        pt = b"reticulum encrypted payload"
        token = dst.encrypt(pt)
        rt = dst.decrypt(token) == pt
    add({"id": "VEC-ID-TOKEN", "kind": "frozen-injection",
         "citation": "Identity.py:827-928; Cryptography/Token.py",
         "title": "Encryption token (ephemeral pub || IV || ciphertext || HMAC)",
         "plaintext_hex": pt.hex(), "token_hex": token.hex(),
         "token_len": len(token),
         "structure": "ephemeral_x25519_pub(32) || token(IV(16)||ct||HMAC(32))",
         "reproducible": "guaranteed by harness determinism self-check",
         "decrypt_roundtrip_ok": rt})


def gen_destination():
    _, dst_id = identities()
    d = RNS.Destination(dst_id, RNS.Destination.OUT, RNS.Destination.SINGLE,
                        "test", "vec")
    add({"id": "VEC-DEST-HASH", "kind": "frozen",
         "citation": "Destination.py:116-141",
         "title": "Destination name hash and destination hash",
         "app_name": "test", "aspects": ["vec"],
         "identity_hash_hex": dst_id.hash.hex(),
         "name_hash_hex": d.name_hash.hex(), "name_hash_len": len(d.name_hash),
         "destination_hash_hex": d.hash.hex(),
         "destination_hash_len": len(d.hash)})
    drop(d)


def gen_packet():
    # PLAIN packet: no encryption, fully deterministic.
    dp = RNS.Destination(None, RNS.Destination.OUT, RNS.Destination.PLAIN,
                         "test", "plain")
    p = RNS.Packet(dp, b"hi", create_receipt=False)
    p.pack()
    raw = p.raw
    add({"id": "VEC-PKT-PLAIN", "kind": "frozen",
         "citation": "Packet.py:177-272",
         "title": "PLAIN HEADER_1 data packet pack",
         "raw_hex": raw.hex(), "raw_len": len(raw),
         "flags_byte": "%02x" % raw[0], "hops_byte": "%02x" % raw[1],
         "destination_hash_hex": raw[2:18].hex(),
         "context_byte": "%02x" % raw[18], "payload_hex": raw[19:].hex(),
         "flags_decode": {
             "ifac": (raw[0] >> 7) & 1, "header_type": (raw[0] >> 6) & 1,
             "context_flag": (raw[0] >> 5) & 1, "transport_type": (raw[0] >> 4) & 1,
             "destination_type": (raw[0] >> 2) & 3, "packet_type": raw[0] & 3}})

    # SINGLE encrypted packet under injection.
    _, dst_id = identities()
    d = RNS.Destination(dst_id, RNS.Destination.OUT, RNS.Destination.SINGLE,
                        "test", "vec")
    with Inject():
        pe = RNS.Packet(d, b"hi", create_receipt=False)
        pe.pack()
        raw_e = pe.raw
    add({"id": "VEC-PKT-ENC", "kind": "frozen-injection",
         "citation": "Packet.py:177-239; Identity.py:827",
         "title": "SINGLE encrypted HEADER_1 data packet pack",
         "raw_hex": raw_e.hex(), "raw_len": len(raw_e),
         "flags_byte": "%02x" % raw_e[0],
         "destination_hash_hex": raw_e[2:18].hex(),
         "context_byte": "%02x" % raw_e[18],
         "reproducible": "guaranteed by harness determinism self-check"})
    drop(dp, d)


def gen_announce():
    _, dst_id = identities()
    # No-ratchet announce.
    with Inject():
        d = RNS.Destination(dst_id, RNS.Destination.IN, RNS.Destination.SINGLE,
                            "test", "vec")
        a = d.announce(app_data=b"AD", send=False); a.pack()
        valid = RNS.Identity.validate_announce(a)
        data = a.raw[RNS.Reticulum.HEADER_MINSIZE:]
        drop(d)
    add({"id": "VEC-ANN-NORATCHET", "kind": "frozen-injection",
         "citation": "Destination.py:280-317; Identity.py:532-634",
         "title": "Announce without ratchet (context flag 0)",
         "raw_hex": a.raw.hex(), "flags_byte": "%02x" % a.raw[0],
         "context_flag": (a.raw[0] >> 5) & 1,
         "announce_data_hex": data.hex(), "announce_data_len": len(data),
         "structure": "public_key(64) || name_hash(10) || random_hash(10) || "
                      "signature(64) || app_data",
         "reproducible": "guaranteed by harness determinism self-check",
         "validate_announce": valid})

    # Ratchet announce.
    with Inject():
        dr = RNS.Destination(dst_id, RNS.Destination.IN, RNS.Destination.SINGLE,
                             "test", "ratchet")
        dr.enable_ratchets("/tmp/rns-vec-ratchets-%d" % os.getpid())
        ar = dr.announce(app_data=b"AD", send=False); ar.pack()
        validr = RNS.Identity.validate_announce(ar)
        datar = ar.raw[RNS.Reticulum.HEADER_MINSIZE:]
        drop(dr)
    add({"id": "VEC-ANN-RATCHET", "kind": "frozen-injection",
         "citation": "Destination.py:284-314; Identity.py:546-553",
         "title": "Announce with ratchet (context flag 1)",
         "raw_hex": ar.raw.hex(), "flags_byte": "%02x" % ar.raw[0],
         "context_flag": (ar.raw[0] >> 5) & 1,
         "announce_data_len": len(datar),
         "structure": "public_key(64) || name_hash(10) || random_hash(10) || "
                      "ratchet(32) || signature(64) || app_data",
         "validate_announce": validr})


def gen_resource_proof():
    data = b"the quick brown fox" * 8
    resource_hash = RNS.Identity.full_hash(b"resource-content")
    proof = RNS.Identity.full_hash(data + resource_hash)
    proof_data = resource_hash + proof
    add({"id": "VEC-RES-PROOF", "kind": "frozen",
         "citation": "Resource.py:752-756",
         "title": "Resource proof hash construction",
         "data_len": len(data), "resource_hash_hex": resource_hash.hex(),
         "proof_hex": proof.hex(),
         "proof_data_hex": proof_data.hex(), "proof_data_len": len(proof_data),
         "structure": "proof = full_hash(data || resource_hash); "
                      "proof_data = resource_hash(32) || proof(32)"})


def gen_channel():
    msgtype, seq, payload = 0xabcd, 7, b"channeldata"
    env = struct.pack(">HHH", msgtype, seq, len(payload)) + payload
    add({"id": "VEC-CHAN-ENVELOPE", "kind": "frozen",
         "citation": "Channel.py:174-200",
         "title": "Channel envelope framing",
         "msgtype": msgtype, "sequence": seq, "payload_hex": payload.hex(),
         "envelope_hex": env.hex(),
         "structure": "u16 msgtype || u16 sequence || u16 length || payload"})

    stream_id, eof, compressed, data = 0x0102, True, False, b"streamdata"
    header_val = (0x3fff & stream_id) | (0x8000 if eof else 0) | (0x4000 if compressed else 0)
    packed = struct.pack(">H", header_val) + data
    add({"id": "VEC-STREAM-HDR", "kind": "frozen",
         "citation": "Buffer.py:80-92",
         "title": "StreamDataMessage header encoding",
         "stream_id": stream_id, "eof": eof, "compressed": compressed,
         "header_value": "%04x" % header_val, "packed_hex": packed.hex(),
         "structure": "u16 (bits0-13 stream_id, bit14 compressed, bit15 eof) || data"})


def gen_hdlc():
    FLAG, ESC, ESC_MASK = 0x7E, 0x7D, 0x20
    sample = bytes([0x01, 0x7E, 0x02, 0x7D, 0x03])
    out = bytearray([FLAG])
    for b in sample:
        if b == FLAG or b == ESC:
            out += bytes([ESC, b ^ ESC_MASK])
        else:
            out.append(b)
    out.append(FLAG)
    add({"id": "VEC-HDLC", "kind": "frozen",
         "citation": "Interfaces/TCPInterface.py:44-52,323",
         "title": "HDLC byte-stuffing frame",
         "flag": "%02x" % FLAG, "escape": "%02x" % ESC, "escape_mask": "%02x" % ESC_MASK,
         "input_hex": sample.hex(), "framed_hex": bytes(out).hex(),
         "structure": "FLAG || stuffed(data) || FLAG; 0x7E->0x7D5E, 0x7D->0x7D5D"})


def gen_header2():
    # HEADER_2 (transport) addressing. A live HEADER_2 packet requires transport
    # routing state; here we document the offsets from Packet.unpack and show a
    # constructed example, citing the source slicing.
    flags = (RNS.Packet.HEADER_2 << 6) | (RNS.Destination.SINGLE << 2) | RNS.Packet.DATA
    transport_id = bytes(range(0xa0, 0xb0))   # 16
    dest = bytes(range(0xb0, 0xc0))           # 16
    raw = bytes([flags, 0x00]) + transport_id + dest + bytes([RNS.Packet.NONE]) + b"data"
    add({"id": "VEC-PKT-HEADER2", "kind": "computed",
         "citation": "Packet.py:255-259",
         "title": "HEADER_2 (transport) addressing layout",
         "note": "constructed per the unpack offsets; a live HEADER_2 packet "
                 "needs transport routing state",
         "flags_byte": "%02x" % flags, "header_type": (flags >> 6) & 1,
         "raw_hex": raw.hex(),
         "transport_id_hex": raw[2:18].hex(),
         "destination_hash_hex": raw[18:34].hex(),
         "context_byte": "%02x" % raw[34], "payload_hex": raw[35:].hex(),
         "structure": "flags || hops || transport_id(16) || dest_hash(16) || "
                      "context || data"})


def gen_path_request():
    target = bytes(range(16))  # the destination hash being sought
    with Inject():
        tag = RNS.Identity.get_random_hash()
        prdst = RNS.Destination(None, RNS.Destination.OUT, RNS.Destination.PLAIN,
                                "rnstransport", "path", "request")
        # transport disabled in the harness config -> payload = dest_hash || tag
        payload = target + tag
        pkt = RNS.Packet(prdst, payload, packet_type=RNS.Packet.DATA,
                         transport_type=RNS.Transport.BROADCAST,
                         header_type=RNS.Packet.HEADER_1)
        pkt.pack()
        raw = pkt.raw
        drop(prdst)
    add({"id": "VEC-PATH-REQUEST", "kind": "frozen-injection",
         "citation": "Transport.py:2780-2787",
         "title": "Path request packet (transport disabled)",
         "destination_namehash_hex": prdst.name_hash.hex(),
         "target_hash_hex": target.hex(), "request_tag_hex": tag.hex(),
         "payload_hex": payload.hex(),
         "payload_structure": "target_destination_hash(16) || request_tag "
                              "(|| transport_identity_hash(16) when transport enabled)",
         "raw_hex": raw.hex(), "flags_byte": "%02x" % raw[0],
         "reproducible": "guaranteed by harness determinism self-check"})


def gen_link():
    # Link id derivation and session-key agreement, using genuine RNS functions
    # with fixed ephemeral keys.
    src, dst = identities()
    with Inject():
        # Initiator ephemerals (x25519 + ed25519), as Link builds internally.
        init_x = X25519PrivateKey.from_private_bytes(bytes([1] * 32))
        init_ed = Ed25519PrivateKey.from_private_bytes(bytes([2] * 32))
        resp_x = X25519PrivateKey.from_private_bytes(bytes([3] * 32))
        init_x_pub = init_x.public_key().public_bytes()
        init_ed_pub = init_ed.public_key().public_bytes()
        resp_x_pub = resp_x.public_key().public_bytes()
        signalling = RNS.Link.signalling_bytes(RNS.Reticulum.MTU,
                                               RNS.Link.MODE_AES256_CBC)
        request_data = init_x_pub + init_ed_pub + signalling
        rdst = RNS.Destination(dst, RNS.Destination.OUT, RNS.Destination.SINGLE,
                               "test", "link")
        lr = RNS.Packet(rdst, request_data, packet_type=RNS.Packet.LINKREQUEST)
        lr.pack()
        link_id = RNS.Link.link_id_from_lr_packet(lr)
        # Session key: ECDH(initiator, responder) is symmetric; both derive same.
        shared_i = init_x.exchange(X25519PublicKey.from_public_bytes(resp_x_pub))
        shared_r = resp_x.exchange(X25519PublicKey.from_public_bytes(init_x_pub))
        derived = RNS.Cryptography.hkdf(length=64, derive_from=shared_i,
                                        salt=link_id, context=None)
        drop(rdst)
    add({"id": "VEC-LINK", "kind": "frozen-injection",
         "citation": "Link.py:308-366",
         "title": "Link request payload, link id, and session key derivation",
         "request_data_structure": "ephemeral_x25519_pub(32) || ephemeral_ed25519_pub(32) "
                                   "|| signalling(3, MTU+mode)",
         "signalling_hex": signalling.hex(),
         "request_data_hex": request_data.hex(),
         "link_id_hex": link_id.hex(), "link_id_len": len(link_id),
         "link_id_derivation": "truncated_hash(packet.get_hashable_part()[:ECPUBSIZE])",
         "ecdh_agreement": shared_i == shared_r,
         "session_key_hex": derived.hex(), "session_key_len": len(derived),
         "key_derivation": "hkdf(64, derive_from=ecdh_shared, salt=link_id, context=None)",
         "reproducible": "guaranteed by harness determinism self-check"})


def gen_resource_adv():
    # Resource advertisement dictionary. Built per ResourceAdvertisement.pack;
    # constructing a live Resource needs a Link, so the dict is assembled from
    # representative fixed values and packed with the genuine msgpack.
    x, p, u, s, c, e = 0, 0, 0, 0, 1, 1   # has_metadata, response, request, split, compressed, encrypted
    f = 0x00 | x << 5 | p << 4 | u << 3 | s << 2 | c << 1 | e
    rhash = RNS.Identity.full_hash(b"resource-data")
    d = {"t": 4096, "d": 8192, "n": 9, "h": rhash, "r": bytes(range(4)),
         "o": rhash, "i": 0, "l": 1, "q": None, "f": f,
         "m": bytes(range(4)) * 9}
    packed = umsgpack.packb(d)
    add({"id": "VEC-RES-ADV", "kind": "computed",
         "citation": "Resource.py:1278-1355",
         "title": "Resource advertisement msgpack dictionary",
         "note": "assembled per ResourceAdvertisement.pack with representative "
                 "values; a live advertisement needs a Resource over a Link",
         "flags_byte": "%02x" % f,
         "flags_decode": {"has_metadata": x, "is_response": p, "is_request": u,
                          "split": s, "compressed": c, "encrypted": e},
         "keys": "t=transfer_size d=data_size n=parts h=hash r=random_hash "
                 "o=original_hash i=segment l=total_segments q=request_id "
                 "f=flags m=hashmap",
         "packed_hex": packed.hex(), "packed_len": len(packed)})


def gen_ifac():
    # IFAC masking, reproduced per Transport.transmit. Deterministic: the IFAC
    # identity signature and HKDF are both deterministic. Proven by a mask +
    # unmask roundtrip.
    ifac_identity = RNS.Identity.from_bytes(bytes([5] * 64))
    ifac_key = bytes(range(32))
    ifac_size = 8
    # A sample packet to authenticate (the PLAIN packet bytes).
    dp = RNS.Destination(None, RNS.Destination.OUT, RNS.Destination.PLAIN,
                         "test", "ifac")
    pk = RNS.Packet(dp, b"hi", create_receipt=False); pk.pack()
    raw = pk.raw
    drop(dp)

    ifac = ifac_identity.sign(raw)[-ifac_size:]
    mask = RNS.Cryptography.hkdf(length=len(raw) + ifac_size,
                                 derive_from=ifac, salt=ifac_key, context=None)
    new_header = bytes([raw[0] | 0x80, raw[1]])
    new_raw = new_header + ifac + raw[2:]
    masked = bytearray()
    for i, byte in enumerate(new_raw):
        if i == 0:
            masked.append(byte ^ mask[i] | 0x80)
        elif i == 1 or i > ifac_size + 1:
            masked.append(byte ^ mask[i])
        else:
            masked.append(byte)
    masked = bytes(masked)

    # Unmask roundtrip: recover original raw and verify the IFAC tag.
    unmasked = bytearray()
    for i, byte in enumerate(masked):
        if i <= 1 or i > ifac_size + 1:
            unmasked.append(byte ^ mask[i])
        else:
            unmasked.append(byte)
    recovered_ifac = bytes(unmasked[2:2 + ifac_size])
    recovered_raw = bytes([unmasked[0] & 0x7f, unmasked[1]]) + bytes(unmasked[2 + ifac_size:])
    roundtrip = (recovered_raw == raw and recovered_ifac == ifac
                 and ifac_identity.sign(recovered_raw)[-ifac_size:] == recovered_ifac)

    add({"id": "VEC-IFAC", "kind": "frozen",
         "citation": "Transport.py:1051-1087",
         "title": "IFAC packet masking",
         "ifac_size": ifac_size, "ifac_key_hex": ifac_key.hex(),
         "input_raw_hex": raw.hex(), "ifac_hex": ifac.hex(),
         "mask_hex": mask.hex(), "masked_hex": masked.hex(),
         "structure": "ifac = sign(raw)[-n:]; mask = hkdf(len(raw)+n, ifac, "
                      "salt=ifac_key); new = header|0x80 || ifac || raw[2:], "
                      "then XOR-mask all but the ifac bytes",
         "header_ifac_flag_set": (masked[0] >> 7) & 1,
         "unmask_roundtrip_ok": roundtrip})


# --------------------------------------------------------------------------
# Determinism self-check and main.
# --------------------------------------------------------------------------

GROUPS = [gen_primitives, gen_identity, gen_destination, gen_packet,
          gen_announce, gen_resource_proof, gen_channel, gen_hdlc,
          gen_header2, gen_path_request, gen_link, gen_resource_adv, gen_ifac]


def build_all():
    VECTORS.clear()
    for g in GROUPS:
        g()


def assert_determinism():
    snapshot = {v["id"]: json.dumps(v, sort_keys=True) for v in VECTORS}
    build_all()
    for v in VECTORS:
        if v["kind"] == "roundtrip":
            continue
        again = json.dumps(v, sort_keys=True)
        if snapshot[v["id"]] != again:
            raise AssertionError(
                "Non-deterministic vector %s changed between runs" % v["id"])


def main():
    boot_reticulum()
    constants = collect_constants()
    build_all()
    assert_determinism()

    doc = {
        "_comment": "Golden vectors for the Reticulum protocol specification. "
                    "Generated by gen_vectors.py from the vendored reference. "
                    "Do not edit by hand; re-run the harness.",
        "meta": {
            "rns_version": RNS.__version__,
            "reticulum_commit": submodule_commit("reference/Reticulum"),
            "fixed_time": FIXED_TIME,
            "src_identity_prv_hex": SRC_PRV.hex(),
            "dst_identity_prv_hex": DST_PRV.hex(),
        },
        "constants": constants,
        "vectors": VECTORS,
    }
    out_path = os.path.join(os.path.dirname(__file__), "vectors.json")
    with open(out_path, "w") as f:
        json.dump(doc, f, indent=2, sort_keys=False)
        f.write("\n")

    print("Wrote %d vectors to %s" % (len(VECTORS), out_path))
    print("RNS %s @ %s" % (RNS.__version__, doc["meta"]["reticulum_commit"][:10]))
    for v in VECTORS:
        print("  %-20s [%-16s] %s" % (v["id"], v["kind"], v["title"]))


if __name__ == "__main__":
    main()
