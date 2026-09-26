#!/usr/bin/env python3
"""
Generate test vectors from Python Reticulum for cross-implementation testing.

Run this script with the Reticulum library available:
    python3 generate_vectors.py

Output: test_vectors.json
"""

import json
import os
import sys

# Add the repo's vendored Reticulum to path (this script lives in scripts/).
_repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(_repo_root, 'reference', 'Reticulum'))

import RNS
from RNS.Cryptography import Token
from RNS.Cryptography import HKDF

def generate_vectors():
    vectors = {}

    # Use fixed seed for reproducibility
    # In real Reticulum, keys are random, but for testing we need determinism
    fixed_prv_bytes = bytes.fromhex(
        # X25519 private key (32 bytes)
        "a8abababababababababababababababababababababababababababababab00"
        # Ed25519 private key (32 bytes)
        "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd01"
    )

    # Create identity from fixed private key
    identity = RNS.Identity(create_keys=False)
    identity.load_private_key(fixed_prv_bytes)

    vectors['identity'] = {
        'private_key': identity.get_private_key().hex(),
        'public_key': identity.get_public_key().hex(),
        'hash': identity.hash.hex(),
    }

    # Test signing
    test_message = b"test message for signing"
    signature = identity.sign(test_message)
    vectors['signing'] = {
        'message': test_message.hex(),
        'signature': signature.hex(),
    }

    # Verify our signature works
    assert identity.validate(signature, test_message), "Signature validation failed!"

    # Test HKDF
    ikm = bytes.fromhex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b")
    salt = bytes.fromhex("000102030405060708090a0b0c")
    info = bytes.fromhex("f0f1f2f3f4f5f6f7f8f9")

    derived = RNS.Cryptography.hkdf(
        length=64,
        derive_from=ikm,
        salt=salt,
        context=info
    )
    vectors['hkdf'] = {
        'ikm': ikm.hex(),
        'salt': salt.hex(),
        'info': info.hex(),
        'length': 64,
        'output': derived.hex(),
    }

    # Test HKDF with None context (as used in Identity)
    derived_no_ctx = RNS.Cryptography.hkdf(
        length=64,
        derive_from=ikm,
        salt=salt,
        context=None
    )
    vectors['hkdf_no_context'] = {
        'ikm': ikm.hex(),
        'salt': salt.hex(),
        'length': 64,
        'output': derived_no_ctx.hex(),
    }

    # Test Token encryption with fixed IV
    token_key = bytes.fromhex(
        "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"  # signing key (32)
        "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100"  # encryption key (32)
    )
    fixed_iv = bytes.fromhex("0f0e0d0c0b0a09080706050403020100")
    plaintext = b"Hello, Reticulum!"

    # We need to manually construct the token since Token.encrypt() uses random IV
    from RNS.Cryptography import PKCS7, AES, HMAC
    from RNS.Cryptography.AES import AES_256_CBC

    signing_key = token_key[:32]
    encryption_key = token_key[32:]

    padded = PKCS7.pad(plaintext)
    ciphertext = AES_256_CBC.encrypt(plaintext=padded, key=encryption_key, iv=fixed_iv)
    signed_parts = fixed_iv + ciphertext
    hmac_value = HMAC.new(signing_key, signed_parts).digest()
    token_bytes = signed_parts + hmac_value

    vectors['token'] = {
        'key': token_key.hex(),
        'iv': fixed_iv.hex(),
        'plaintext': plaintext.hex(),
        'ciphertext': ciphertext.hex(),
        'token': token_bytes.hex(),
    }

    # Verify token can be decrypted
    token_obj = Token(token_key)
    decrypted = token_obj.decrypt(token_bytes)
    assert decrypted == plaintext, "Token decryption failed!"

    # Test Identity encryption with fixed ephemeral key
    # This is tricky because Identity.encrypt() generates random ephemeral key
    # We'll construct it manually to get deterministic output

    from RNS.Cryptography import X25519PrivateKey, X25519PublicKey

    # Fixed ephemeral private key
    ephemeral_prv_bytes = bytes.fromhex("b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b802")
    ephemeral_prv = X25519PrivateKey.from_private_bytes(ephemeral_prv_bytes)
    ephemeral_pub_bytes = ephemeral_prv.public_key().public_bytes()

    # Perform ECDH with identity's public key
    target_pub = X25519PublicKey.from_public_bytes(identity.pub_bytes)
    shared_key = ephemeral_prv.exchange(target_pub)

    # Derive key using HKDF
    derived_key = RNS.Cryptography.hkdf(
        length=64,
        derive_from=shared_key,
        salt=identity.hash,  # identity hash as salt
        context=None
    )

    # Encrypt with Token using fixed IV
    encrypt_plaintext = b"Secret message for identity"
    signing_key_enc = derived_key[:32]
    encryption_key_enc = derived_key[32:]

    fixed_iv_enc = bytes.fromhex("1a2b3c4d5e6f708192a3b4c5d6e7f801")
    padded_enc = PKCS7.pad(encrypt_plaintext)
    ciphertext_enc = AES_256_CBC.encrypt(plaintext=padded_enc, key=encryption_key_enc, iv=fixed_iv_enc)
    signed_parts_enc = fixed_iv_enc + ciphertext_enc
    hmac_enc = HMAC.new(signing_key_enc, signed_parts_enc).digest()
    token_ciphertext = signed_parts_enc + hmac_enc

    # Full encrypted token: ephemeral_pub + token
    full_ciphertext = ephemeral_pub_bytes + token_ciphertext

    vectors['identity_encrypt'] = {
        'identity_public_key': identity.get_public_key().hex(),
        'identity_hash': identity.hash.hex(),
        'ephemeral_private_key': ephemeral_prv_bytes.hex(),
        'ephemeral_public_key': ephemeral_pub_bytes.hex(),
        'shared_key': shared_key.hex(),
        'derived_key': derived_key.hex(),
        'plaintext': encrypt_plaintext.hex(),
        'iv': fixed_iv_enc.hex(),
        'token_ciphertext': token_ciphertext.hex(),
        'full_ciphertext': full_ciphertext.hex(),
    }

    # Verify we can decrypt it
    decrypted_msg = identity.decrypt(full_ciphertext)
    assert decrypted_msg == encrypt_plaintext, f"Identity decryption failed! Got: {decrypted_msg}"

    # Test destination hash calculation
    app_name = "test"
    aspects = ["echo", "request"]

    # Calculate name hash manually (first 10 bytes of SHA256)
    full_name = app_name + "." + ".".join(aspects)
    name_hash = RNS.Identity.full_hash(full_name.encode())[:10]

    # Destination hash = truncated_hash(name_hash + identity_hash)
    hash_material = name_hash + identity.hash
    dest_hash = RNS.Identity.truncated_hash(hash_material)

    vectors['destination'] = {
        'app_name': app_name,
        'aspects': aspects,
        'full_name': full_name,
        'name_hash': name_hash.hex(),
        'identity_hash': identity.hash.hex(),
        'destination_hash': dest_hash.hex(),
    }

    # ==========================================================
    # Packet serialization test vectors
    # ==========================================================
    from RNS import Packet

    # Test flags byte encoding
    # Python: flags = (header_type << 6) | (context_flag << 5) | (transport_type << 4) | (dest_type << 2) | packet_type
    flags_tests = []

    # Test case 1: Basic DATA packet, HEADER_1, broadcast, SINGLE destination
    # header_type=0, context_flag=0, transport_type=0, dest_type=0 (SINGLE), packet_type=0 (DATA)
    flags1 = (0 << 6) | (0 << 5) | (0 << 4) | (0 << 2) | 0
    flags_tests.append({
        'name': 'data_h1_broadcast_single',
        'header_type': 0,
        'context_flag': 0,
        'transport_type': 0,
        'dest_type': 0,  # SINGLE
        'packet_type': 0,  # DATA
        'expected_flags': flags1,
    })

    # Test case 2: ANNOUNCE packet, HEADER_1, broadcast, SINGLE
    flags2 = (0 << 6) | (0 << 5) | (0 << 4) | (0 << 2) | 1
    flags_tests.append({
        'name': 'announce_h1_broadcast_single',
        'header_type': 0,
        'context_flag': 0,
        'transport_type': 0,
        'dest_type': 0,
        'packet_type': 1,  # ANNOUNCE
        'expected_flags': flags2,
    })

    # Test case 3: LINKREQUEST packet, HEADER_1, broadcast, SINGLE
    flags3 = (0 << 6) | (0 << 5) | (0 << 4) | (0 << 2) | 2
    flags_tests.append({
        'name': 'linkrequest_h1_broadcast_single',
        'header_type': 0,
        'context_flag': 0,
        'transport_type': 0,
        'dest_type': 0,
        'packet_type': 2,  # LINKREQUEST
        'expected_flags': flags3,
    })

    # Test case 4: PROOF packet, HEADER_1, broadcast, LINK destination
    flags4 = (0 << 6) | (0 << 5) | (0 << 4) | (2 << 2) | 3
    flags_tests.append({
        'name': 'proof_h1_broadcast_link',
        'header_type': 0,
        'context_flag': 0,
        'transport_type': 0,
        'dest_type': 2,  # LINK
        'packet_type': 3,  # PROOF
        'expected_flags': flags4,
    })

    # Test case 5: DATA with HEADER_2 (transport routing)
    flags5 = (1 << 6) | (0 << 5) | (1 << 4) | (0 << 2) | 0
    flags_tests.append({
        'name': 'data_h2_transport_single',
        'header_type': 1,  # HEADER_2
        'context_flag': 0,
        'transport_type': 1,  # TRANSPORT
        'dest_type': 0,
        'packet_type': 0,
        'expected_flags': flags5,
    })

    # Test case 6: DATA with context flag set
    flags6 = (0 << 6) | (1 << 5) | (0 << 4) | (0 << 2) | 0
    flags_tests.append({
        'name': 'data_h1_context_set',
        'header_type': 0,
        'context_flag': 1,  # Context flag set
        'transport_type': 0,
        'dest_type': 0,
        'packet_type': 0,
        'expected_flags': flags6,
    })

    # Test case 7: GROUP destination
    flags7 = (0 << 6) | (0 << 5) | (0 << 4) | (1 << 2) | 0
    flags_tests.append({
        'name': 'data_h1_group',
        'header_type': 0,
        'context_flag': 0,
        'transport_type': 0,
        'dest_type': 1,  # GROUP
        'packet_type': 0,
        'expected_flags': flags7,
    })

    # Test case 8: All bits set (except transport which only has values 0/1)
    flags8 = (1 << 6) | (1 << 5) | (1 << 4) | (3 << 2) | 3
    flags_tests.append({
        'name': 'all_high_bits',
        'header_type': 1,
        'context_flag': 1,
        'transport_type': 1,
        'dest_type': 3,  # PLAIN (3)
        'packet_type': 3,  # PROOF
        'expected_flags': flags8,
    })

    vectors['packet_flags'] = flags_tests

    # Test complete packet serialization (manual construction matching Python format)
    # Packet format: [flags(1)][hops(1)][dest_hash(16)][context(1)][data(N)]
    packet_tests = []

    # Simple DATA packet
    test_dest_hash = bytes.fromhex("01020304050607080910111213141516")
    test_data = b"Hello"
    test_context = 0x00  # NONE

    # Build packet manually matching Python's pack() method
    packet1_flags = (0 << 6) | (0 << 5) | (0 << 4) | (0 << 2) | 0  # DATA, H1, broadcast, SINGLE
    packet1_raw = bytes([packet1_flags, 0]) + test_dest_hash + bytes([test_context]) + test_data

    packet_tests.append({
        'name': 'simple_data_packet',
        'flags': packet1_flags,
        'hops': 0,
        'header_type': 0,
        'dest_hash': test_dest_hash.hex(),
        'context': test_context,
        'data': test_data.hex(),
        'raw_packet': packet1_raw.hex(),
    })

    # LINKREQUEST packet (64 byte payload)
    linkrequest_payload = bytes(64)  # 64 zero bytes for testing
    packet2_flags = (0 << 6) | (0 << 5) | (0 << 4) | (0 << 2) | 2  # LINKREQUEST
    packet2_raw = bytes([packet2_flags, 0]) + test_dest_hash + bytes([0x00]) + linkrequest_payload

    packet_tests.append({
        'name': 'linkrequest_packet',
        'flags': packet2_flags,
        'hops': 0,
        'header_type': 0,
        'dest_hash': test_dest_hash.hex(),
        'context': 0x00,
        'data': linkrequest_payload.hex(),
        'raw_packet': packet2_raw.hex(),
    })

    # Packet with HEADER_2 (transport routing)
    test_transport_id = bytes.fromhex("a1a2a3a4a5a6a7a8a9aaabacadaeafa0")
    packet3_flags = (1 << 6) | (0 << 5) | (1 << 4) | (0 << 2) | 0  # DATA, H2, transport, SINGLE
    packet3_raw = bytes([packet3_flags, 5]) + test_transport_id + test_dest_hash + bytes([0x00]) + test_data

    packet_tests.append({
        'name': 'data_packet_h2',
        'flags': packet3_flags,
        'hops': 5,
        'header_type': 1,
        'transport_id': test_transport_id.hex(),
        'dest_hash': test_dest_hash.hex(),
        'context': 0x00,
        'data': test_data.hex(),
        'raw_packet': packet3_raw.hex(),
    })

    # Packet with keepalive context
    packet4_flags = (0 << 6) | (1 << 5) | (0 << 4) | (2 << 2) | 0  # DATA, context_flag, LINK dest
    packet4_raw = bytes([packet4_flags, 0]) + test_dest_hash + bytes([0xFA]) + b""  # KEEPALIVE context, empty data

    packet_tests.append({
        'name': 'keepalive_packet',
        'flags': packet4_flags,
        'hops': 0,
        'header_type': 0,
        'dest_hash': test_dest_hash.hex(),
        'context': 0xFA,  # KEEPALIVE
        'data': "",
        'raw_packet': packet4_raw.hex(),
    })

    vectors['packets'] = packet_tests

    # ==========================================================
    # Link request/proof test vectors
    # ==========================================================

    # Generate link request data
    link_ephemeral_prv = bytes.fromhex("c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c803")
    link_ephemeral_pub = X25519PrivateKey.from_private_bytes(link_ephemeral_prv).public_key().public_bytes()

    # Ed25519 signing key for the link
    from RNS.Cryptography.Ed25519 import Ed25519PrivateKey
    link_signing_seed = bytes.fromhex("d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d804")
    link_signing_key = Ed25519PrivateKey.from_private_bytes(link_signing_seed)
    link_verifying_pub = link_signing_key.public_key().public_bytes()

    # Link request payload: [ephemeral_pub(32)][signing_pub(32)] = 64 bytes
    link_request_data = link_ephemeral_pub + link_verifying_pub

    vectors['link_request'] = {
        'ephemeral_private_key': link_ephemeral_prv.hex(),
        'ephemeral_public_key': link_ephemeral_pub.hex(),
        'signing_seed': link_signing_seed.hex(),
        'verifying_public_key': link_verifying_pub.hex(),
        'request_data': link_request_data.hex(),
    }

    # Link ID calculation
    # Python: link_id = truncated_hash((flags & 0x0F) + raw[2:])
    # where raw[2:] is everything after flags and hops
    link_dest_hash = bytes.fromhex("e1e2e3e4e5e6e7e8e9eaebecedeeefe0")
    link_packet_flags = 0x02  # LINKREQUEST, H1, broadcast, SINGLE
    link_raw_packet = bytes([link_packet_flags, 0]) + link_dest_hash + bytes([0x00]) + link_request_data

    # Calculate link_id as Python does
    hashable_part = bytes([link_packet_flags & 0x0F]) + link_raw_packet[2:]
    link_id = RNS.Identity.truncated_hash(hashable_part)

    vectors['link_id'] = {
        'raw_packet': link_raw_packet.hex(),
        'hashable_part': hashable_part.hex(),
        'link_id': link_id.hex(),
    }

    # ==========================================================
    # Announce packet test vectors
    # ==========================================================

    # Announce format (without ratchet):
    # [public_key (64)] [name_hash (10)] [random_hash (10)] [signature (64)] [app_data (var)]
    # Signature signs: dest_hash + public_key + name_hash + random_hash + app_data

    # Use our fixed identity from earlier
    announce_public_key = identity.get_public_key()  # 64 bytes

    # Calculate name hash (first 10 bytes of SHA256 of full_name)
    announce_app_name = "myapp"
    announce_aspects = ["service", "echo"]
    announce_full_name = announce_app_name + "." + ".".join(announce_aspects)
    announce_name_hash = RNS.Identity.full_hash(announce_full_name.encode())[:10]

    # Fixed random hash for deterministic testing
    announce_random_hash = bytes.fromhex("f1f2f3f4f5f6f7f8f9fa")

    # Calculate destination hash = truncated_hash(name_hash + identity_hash)
    announce_hash_material = announce_name_hash + identity.hash
    announce_dest_hash = RNS.Identity.truncated_hash(announce_hash_material)

    # App data (optional, but include for testing)
    announce_app_data = b"Hello from Reticulum!"

    # Build signed data: dest_hash + public_key + name_hash + random_hash + app_data
    # Note: For announces without ratchet, no ratchet bytes are included
    announce_signed_data = announce_dest_hash + announce_public_key + announce_name_hash + announce_random_hash + announce_app_data

    # Sign with identity
    announce_signature = identity.sign(announce_signed_data)

    # Build announce payload: public_key + name_hash + random_hash + signature + app_data
    announce_payload = announce_public_key + announce_name_hash + announce_random_hash + announce_signature + announce_app_data

    # Verify our announce is valid
    # Reconstruct signed data from payload
    verify_public_key = announce_payload[:64]
    verify_name_hash = announce_payload[64:74]
    verify_random_hash = announce_payload[74:84]
    verify_signature = announce_payload[84:148]
    verify_app_data = announce_payload[148:]
    verify_signed_data = announce_dest_hash + verify_public_key + verify_name_hash + verify_random_hash + verify_app_data
    assert identity.validate(verify_signature, verify_signed_data), "Announce signature validation failed!"

    vectors['announce'] = {
        'app_name': announce_app_name,
        'aspects': announce_aspects,
        'full_name': announce_full_name,
        'name_hash': announce_name_hash.hex(),
        'random_hash': announce_random_hash.hex(),
        'identity_hash': identity.hash.hex(),
        'destination_hash': announce_dest_hash.hex(),
        'public_key': announce_public_key.hex(),
        'signed_data': announce_signed_data.hex(),
        'signature': announce_signature.hex(),
        'app_data': announce_app_data.hex(),
        'payload': announce_payload.hex(),
    }

    # Also test announce with empty app_data
    announce_signed_data_empty = announce_dest_hash + announce_public_key + announce_name_hash + announce_random_hash
    announce_signature_empty = identity.sign(announce_signed_data_empty)
    announce_payload_empty = announce_public_key + announce_name_hash + announce_random_hash + announce_signature_empty

    vectors['announce_no_app_data'] = {
        'destination_hash': announce_dest_hash.hex(),
        'signed_data': announce_signed_data_empty.hex(),
        'signature': announce_signature_empty.hex(),
        'payload': announce_payload_empty.hex(),
    }

    # ==========================================================
    # Link establishment test vectors
    # ==========================================================

    # Link establishment involves:
    # 1. Initiator sends LINKREQUEST with [ephemeral_X25519_pub (32)] [ephemeral_Ed25519_pub (32)]
    # 2. Responder sends PROOF with [signature (64)] [ephemeral_X25519_pub (32)] [signalling (3)]
    # 3. Both derive shared key via ECDH + HKDF

    # Initiator's ephemeral keys (reuse from earlier link_request vectors)
    initiator_x25519_prv = bytes.fromhex("c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c8c803")
    initiator_x25519_pub = X25519PrivateKey.from_private_bytes(initiator_x25519_prv).public_key().public_bytes()
    initiator_ed25519_seed = bytes.fromhex("d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d8d804")
    initiator_ed25519_prv = Ed25519PrivateKey.from_private_bytes(initiator_ed25519_seed)
    initiator_ed25519_pub = initiator_ed25519_prv.public_key().public_bytes()

    # Responder's ephemeral keys (use identity for signing, new X25519 for ECDH)
    responder_x25519_prv = bytes.fromhex("e8e8e8e8e8e8e8e8e8e8e8e8e8e8e8e8e8e8e8e8e8e8e8e8e8e8e8e8e8e8e805")
    responder_x25519_key = X25519PrivateKey.from_private_bytes(responder_x25519_prv)
    responder_x25519_pub = responder_x25519_key.public_key().public_bytes()
    # Responder uses destination's identity for signing (the 'identity' object)

    # Link ID (from earlier calculation)
    link_id_bytes = bytes.fromhex(vectors['link_id']['link_id'])

    # Signalling bytes (MTU and mode)
    # Format: 21-bit MTU + 3-bit mode, packed as 3 bytes big-endian
    # MTU=500 (0x1F4), mode=1 -> signalling = 0x1F4 | (1 << 21) = 0x2001F4
    link_mtu = 500
    link_mode = 1
    link_signalling_value = (link_mtu & 0x1FFFFF) | ((link_mode & 0x07) << 21)
    link_signalling_bytes = link_signalling_value.to_bytes(4, 'big')[1:]  # Take last 3 bytes

    # Signed data for proof: link_id + initiator_X25519_pub + initiator_Ed25519_pub + signalling
    # Note: In Reticulum, the responder signs the initiator's public keys
    proof_signed_data = link_id_bytes + initiator_x25519_pub + initiator_ed25519_pub + link_signalling_bytes

    # Responder signs with their identity (destination's identity)
    proof_signature = identity.sign(proof_signed_data)

    # Link proof payload: [signature (64)] [responder_X25519_pub (32)] [signalling (3)]
    proof_payload = proof_signature + responder_x25519_pub + link_signalling_bytes

    # Key derivation: ECDH + HKDF
    # Initiator performs: ECDH(initiator_prv, responder_pub)
    # Responder performs: ECDH(responder_prv, initiator_pub)
    # Both should get the same shared_key

    # Calculate shared key (from responder's perspective, same result either way)
    initiator_x25519_key = X25519PrivateKey.from_private_bytes(initiator_x25519_prv)
    responder_pub_obj = X25519PublicKey.from_public_bytes(responder_x25519_pub)
    initiator_pub_obj = X25519PublicKey.from_public_bytes(initiator_x25519_pub)

    # Shared key (both sides compute the same value)
    shared_key = initiator_x25519_key.exchange(responder_pub_obj)

    # Derive link key using HKDF
    # salt = link_id, context = None, length = 64 (for AES-256-CBC)
    derived_key = RNS.Cryptography.hkdf(
        length=64,
        derive_from=shared_key,
        salt=link_id_bytes,
        context=None
    )

    vectors['link_proof'] = {
        'link_id': link_id_bytes.hex(),
        'initiator_x25519_private': initiator_x25519_prv.hex(),
        'initiator_x25519_public': initiator_x25519_pub.hex(),
        'initiator_ed25519_seed': initiator_ed25519_seed.hex(),
        'initiator_ed25519_public': initiator_ed25519_pub.hex(),
        'responder_x25519_private': responder_x25519_prv.hex(),
        'responder_x25519_public': responder_x25519_pub.hex(),
        'responder_signing_public': identity.get_public_key()[32:].hex(),  # Ed25519 part only
        'signalling_bytes': link_signalling_bytes.hex(),
        'signed_data': proof_signed_data.hex(),
        'signature': proof_signature.hex(),
        'proof_payload': proof_payload.hex(),
        'shared_key': shared_key.hex(),
        'derived_key': derived_key.hex(),
    }

    # Verify the proof signature
    assert identity.validate(proof_signature, proof_signed_data), "Link proof signature validation failed!"

    # Verify shared key from both perspectives
    shared_key_from_responder = responder_x25519_key.exchange(initiator_pub_obj)
    assert shared_key == shared_key_from_responder, "Shared key mismatch between perspectives!"

    # ==========================================================
    # HDLC framing test vectors
    # ==========================================================

    # Reticulum uses simplified HDLC framing (no CRC):
    # Format: [FLAG (0x7E)] [Escaped Data] [FLAG (0x7E)]
    # Escaping: FLAG (0x7E) -> ESCAPE (0x7D) XOR 0x20 = 0x7D 0x5E
    #           ESCAPE (0x7D) -> 0x7D 0x5D

    FLAG = 0x7E
    ESCAPE = 0x7D
    ESCAPE_XOR = 0x20

    def hdlc_frame(data):
        """Frame data with simplified HDLC encoding"""
        output = bytes([FLAG])
        for byte in data:
            if byte == FLAG or byte == ESCAPE:
                output += bytes([ESCAPE, byte ^ ESCAPE_XOR])
            else:
                output += bytes([byte])
        output += bytes([FLAG])
        return output

    # Test case 1: Simple data (no escaping needed)
    hdlc_simple_data = b"Hello"
    hdlc_simple_framed = hdlc_frame(hdlc_simple_data)

    # Test case 2: Data containing FLAG byte (0x7E)
    hdlc_flag_data = bytes([0x00, FLAG, 0xFF])
    hdlc_flag_framed = hdlc_frame(hdlc_flag_data)

    # Test case 3: Data containing ESCAPE byte (0x7D)
    hdlc_escape_data = bytes([0x00, ESCAPE, 0xFF])
    hdlc_escape_framed = hdlc_frame(hdlc_escape_data)

    # Test case 4: Data containing both FLAG and ESCAPE
    hdlc_both_data = bytes([FLAG, 0x00, ESCAPE, 0xFF])
    hdlc_both_framed = hdlc_frame(hdlc_both_data)

    # Test case 5: A real packet (use our simple data packet from earlier)
    hdlc_packet_data = bytes.fromhex(vectors['packets'][0]['raw_packet'])
    hdlc_packet_framed = hdlc_frame(hdlc_packet_data)

    vectors['hdlc'] = {
        'flag': FLAG,
        'escape': ESCAPE,
        'escape_xor': ESCAPE_XOR,
        'simple': {
            'data': hdlc_simple_data.hex(),
            'framed': hdlc_simple_framed.hex(),
        },
        'with_flag': {
            'data': hdlc_flag_data.hex(),
            'framed': hdlc_flag_framed.hex(),
        },
        'with_escape': {
            'data': hdlc_escape_data.hex(),
            'framed': hdlc_escape_framed.hex(),
        },
        'with_both': {
            'data': hdlc_both_data.hex(),
            'framed': hdlc_both_framed.hex(),
        },
        'packet': {
            'data': hdlc_packet_data.hex(),
            'framed': hdlc_packet_framed.hex(),
        },
    }

    # ==========================================================
    # IFAC (Interface Access Code) test vectors
    # ==========================================================

    # IFAC constants from Reticulum
    IFAC_SALT = bytes.fromhex("adf54d882c9a9b80771eb4995d702d4a3e733391b2a0f53f416d9f907e55cff8")

    # Test configuration: netname and netkey
    ifac_netname = "testnet"
    ifac_netkey = "secretkey123"
    ifac_size = 16

    # Key derivation process (matching Reticulum.py lines 808-829)
    # 1. Hash netname and netkey separately
    ifac_netname_hash = RNS.Identity.full_hash(ifac_netname.encode("utf-8"))
    ifac_netkey_hash = RNS.Identity.full_hash(ifac_netkey.encode("utf-8"))

    # 2. Concatenate hashes
    ifac_origin = ifac_netname_hash + ifac_netkey_hash

    # 3. Hash the combined origin
    ifac_origin_hash = RNS.Identity.full_hash(ifac_origin)

    # 4. Derive 64-byte key using HKDF
    ifac_key = RNS.Cryptography.hkdf(
        length=64,
        derive_from=ifac_origin_hash,
        salt=IFAC_SALT,
        context=None
    )

    # 5. Create identity from derived key
    ifac_identity = RNS.Identity.from_bytes(ifac_key)

    # Test packet for IFAC masking
    ifac_test_packet = bytes.fromhex("0001020304050607080910111213141516171819")

    # Apply IFAC masking (matching Transport.py lines 895-930)
    # Step 1: Sign packet and take last N bytes
    ifac_signature = ifac_identity.sign(ifac_test_packet)
    ifac_value = ifac_signature[-ifac_size:]

    # Step 2: Generate mask
    ifac_mask = RNS.Cryptography.hkdf(
        length=len(ifac_test_packet) + ifac_size,
        derive_from=ifac_value,
        salt=ifac_key,
        context=None
    )

    # Step 3: Build packet with IFAC flag and inserted IFAC bytes
    # Structure: [flags(1) with bit 7 set][hops(1)][ifac(N)][rest of packet...]
    ifac_new_header = bytes([ifac_test_packet[0] | 0x80, ifac_test_packet[1]])
    ifac_new_raw = ifac_new_header + ifac_value + ifac_test_packet[2:]

    # Step 4: Apply XOR mask (matching exact Python logic)
    ifac_masked = b""
    for i, byte in enumerate(ifac_new_raw):
        if i == 0:
            # Mask first byte but keep IFAC flag set
            ifac_masked += bytes([byte ^ ifac_mask[i] | 0x80])
        elif i == 1 or i > ifac_size + 1:
            # Mask second header byte and payload (after IFAC)
            ifac_masked += bytes([byte ^ ifac_mask[i]])
        else:
            # Don't mask IFAC bytes themselves
            ifac_masked += bytes([byte])

    # Verify by unmasking and checking signature
    def verify_ifac(raw, ifac_identity, ifac_key, ifac_size):
        """Verify IFAC on a packet (simplified version of Transport.inbound)"""
        if raw[0] & 0x80 != 0x80:
            return None  # No IFAC flag

        # Extract IFAC
        ifac = raw[2:2+ifac_size]

        # Generate mask
        mask = RNS.Cryptography.hkdf(
            length=len(raw),
            derive_from=ifac,
            salt=ifac_key,
            context=None
        )

        # Unmask
        unmasked = b""
        for i, byte in enumerate(raw):
            if i <= 1 or i > ifac_size + 1:
                unmasked += bytes([byte ^ mask[i]])
            else:
                unmasked += bytes([byte])

        # Clear IFAC flag and remove IFAC bytes
        clean = bytes([unmasked[0] & 0x7F, unmasked[1]]) + unmasked[2+ifac_size:]

        # Verify signature
        expected_ifac = ifac_identity.sign(clean)[-ifac_size:]
        if ifac == expected_ifac:
            return clean
        return None

    # Verify our masked packet
    ifac_verified = verify_ifac(ifac_masked, ifac_identity, ifac_key, ifac_size)
    assert ifac_verified == ifac_test_packet, f"IFAC verification failed! Got: {ifac_verified.hex() if ifac_verified else 'None'}"

    vectors['ifac'] = {
        'netname': ifac_netname,
        'netkey': ifac_netkey,
        'ifac_size': ifac_size,
        'ifac_salt': IFAC_SALT.hex(),
        'netname_hash': ifac_netname_hash.hex(),
        'netkey_hash': ifac_netkey_hash.hex(),
        'ifac_origin': ifac_origin.hex(),
        'ifac_origin_hash': ifac_origin_hash.hex(),
        'ifac_key': ifac_key.hex(),
        'ifac_identity_hash': ifac_identity.hash.hex(),
        'test_packet': ifac_test_packet.hex(),
        'signature': ifac_signature.hex(),
        'ifac_value': ifac_value.hex(),
        'mask': ifac_mask.hex(),
        'masked_packet': ifac_masked.hex(),
    }

    # Also test with netname only
    ifac_netname_only_hash = RNS.Identity.full_hash("netonly".encode("utf-8"))
    ifac_netname_only_origin_hash = RNS.Identity.full_hash(ifac_netname_only_hash)
    ifac_netname_only_key = RNS.Cryptography.hkdf(
        length=64,
        derive_from=ifac_netname_only_origin_hash,
        salt=IFAC_SALT,
        context=None
    )

    vectors['ifac_netname_only'] = {
        'netname': "netonly",
        'ifac_origin_hash': ifac_netname_only_origin_hash.hex(),
        'ifac_key': ifac_netname_only_key.hex(),
    }

    return vectors


if __name__ == '__main__':
    print("Generating test vectors...")
    vectors = generate_vectors()

    output_path = os.path.join(os.path.dirname(__file__), '..', 'leviculum-core', 'tests', 'vectors', 'test_vectors.json')
    with open(output_path, 'w') as f:
        json.dump(vectors, f, indent=2)

    print(f"Wrote test vectors to: {output_path}")
    print(f"Vectors generated: {list(vectors.keys())}")
