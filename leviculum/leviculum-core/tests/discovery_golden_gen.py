#!/usr/bin/env python3
# Regenerate the discovery wire-format golden vectors embedded in
# src/discovery/tests.rs from the vendored Python reference. Run from the repo
# root:
#
#   PYTHONPATH=reference/Reticulum:reference/LXMF python3 \
#       leviculum-core/tests/discovery_golden_gen.py
#
# Emits, per vector: packed(msgpack) hex, full app_data hex, stamp value,
# discovery_hash. The stamp is random per run; regenerate all four fields
# together when refreshing a vector.

import RNS
from RNS.vendor import umsgpack as msgpack
from LXMF import LXStamper
from RNS.Discovery import InterfaceAnnounceHandler

NAME = 0xFF; TRANSPORT_ID = 0xFE; INTERFACE_TYPE = 0x00; TRANSPORT = 0x01
REACHABLE_ON = 0x02; LATITUDE = 0x03; LONGITUDE = 0x04; HEIGHT = 0x05; PORT = 0x06
IFAC_NETNAME = 0x07; IFAC_NETKEY = 0x08; FREQUENCY = 0x09; BANDWIDTH = 0x0A
SPREADINGFACTOR = 0x0B; CODINGRATE = 0x0C
ROUNDS = 20
COST = 14

TID_HEX = "00112233445566778899aabbccddeeff"


def full_hash(b):
    return RNS.Identity.full_hash(b)


def discovery_hash(name):
    return full_hash((TID_HEX + name).encode("utf-8")).hex()


def emit(label, info, effective_name):
    packed = msgpack.packb(info)
    infohash = full_hash(packed)
    workblock = LXStamper.stamp_workblock(infohash, expand_rounds=ROUNDS)
    stamp, value = LXStamper.generate_stamp(infohash, stamp_cost=COST, expand_rounds=ROUNDS)
    assert LXStamper.stamp_valid(stamp, COST, workblock)
    app = bytes([0x00]) + packed + stamp
    print(f"=== {label} ===")
    print("packed        ", packed.hex())
    print("app           ", app.hex())
    print("stamp         ", stamp.hex())
    print("value         ", value)
    print("discovery_hash", discovery_hash(effective_name))
    print()


tid = bytes.fromhex(TID_HEX)

emit("A_rnode", {
    INTERFACE_TYPE: "RNodeInterface", TRANSPORT: True, TRANSPORT_ID: tid,
    NAME: "Test RNode", LATITUDE: None, LONGITUDE: None, HEIGHT: None,
    FREQUENCY: 868000000, BANDWIDTH: 125000, SPREADINGFACTOR: 8, CODINGRATE: 5,
}, "Test RNode")

emit("B_tcpserver", {
    INTERFACE_TYPE: "TCPServerInterface", TRANSPORT: False, TRANSPORT_ID: tid,
    NAME: "Backbone Node", LATITUDE: 55.6761, LONGITUDE: 12.5683, HEIGHT: 10.5,
    REACHABLE_ON: "example.com", PORT: 4242,
}, "Backbone Node")

emit("C_backbone", {
    INTERFACE_TYPE: "BackboneInterface", TRANSPORT: True, TRANSPORT_ID: tid,
    NAME: None, LATITUDE: None, LONGITUDE: None, HEIGHT: None,
    REACHABLE_ON: "10.0.0.1", PORT: 4965,
    IFAC_NETNAME: "mynet", IFAC_NETKEY: "secretkey",
}, "Discovered BackboneInterface")

# Primitive stamp vector (material = 0xaa*32).
mat = bytes.fromhex("aa" * 32)
wb = LXStamper.stamp_workblock(mat, expand_rounds=ROUNDS)
print("=== PRIM ===")
print("material      ", mat.hex())
print("wb_full_hash  ", full_hash(wb).hex())
print("zero_stamp_val", LXStamper.stamp_value(wb, bytes(32)))

# Encrypted discovery announce (Codeberg #32 sub-task d). A private discovery
# network shares a `network_identity` and encrypts `packed+stamp` with it
# (Discovery.py get_interface_announce_data). The ephemeral key is random per
# run, so A_ENC_APP is not reproducible byte-for-byte; decryption is
# deterministic, so it decodes to vector A on any stack holding the identity.
from RNS.Discovery import InterfaceAnnounceHandler

NET_PRV = bytes(range(64))  # fixed 32 X25519 + 32 Ed25519 private key
net = RNS.Identity(create_keys=False)
net.load_private_key(NET_PRV)

A_PACKED = bytes.fromhex(
    "8b00ae524e6f6465496e7465726661636501c3ccfec41000112233445566778899aabbccddeeff"
    "ccffaa5465737420524e6f646503c004c005c009ce33bca1000ace0001e8480b080c05"
)
A_STAMP = bytes.fromhex("b429796794e50f63f5c02a4e4a458434399a6d356f43e35a26d654bcfd1e583e")
enc_app = bytes([InterfaceAnnounceHandler.FLAG_ENCRYPTED]) + net.encrypt(A_PACKED + A_STAMP)
print("=== ENC (A) ===")
print("net_prv       ", NET_PRV.hex())
print("net_hash      ", net.hash.hex())
print("flag_encrypted", InterfaceAnnounceHandler.FLAG_ENCRYPTED)
print("enc_app       ", enc_app.hex())
