# Cryptographic primitives

LXMF builds on Reticulum primitives for all hashing, signing, and encryption. An
implementation MUST use primitives that produce byte-identical results to these,
because their outputs are signed, hashed, and exchanged on the wire.

## Hashing

- **`full_hash(x)`** is SHA-256 over `x`, 32 bytes (`RNS.Identity.HASHLENGTH` =
  256 bits). LXMF uses it for the message hash and message-id
  (`LXMessage.py:365-366`), the transient-id (`LXMessage.py:431`), and the stamp
  digest (`LXStamper.py:34,44`).
- **`truncated_hash(x)`** is the leading 16 bytes of `full_hash(x)`
  (`RNS.Identity.TRUNCATED_HASHLENGTH` = 128 bits). LXMF uses it for the ticket
  stamp shortcut (`LXMessage.py:274,297`).

## Signing

- **`Identity.sign(m)`** is Ed25519 over `m`, producing a 64-byte signature
  (`RNS.Identity.SIGLENGTH` = 512 bits). Ed25519 is deterministic (RFC 8032), so
  a given key and message always yield the same signature; `[VEC-MSG-1]` pins
  one.
- **`Identity.validate(sig, m)`** verifies an Ed25519 signature. The inbound
  path calls it as `source.identity.validate(signature, signed_part)`
  (`LXMessage.py:794`).

## Encryption

- **`Destination.encrypt(plaintext)`** encrypts to a SINGLE destination using
  Reticulum's ECDH scheme: a fresh ephemeral X25519 key per call, an HKDF-derived
  AES-128-CBC key, and an HMAC token, optionally keyed by the destination's
  current ratchet. Because the ephemeral key is fresh per call, the ciphertext is
  **not** reproducible across runs; LXMF vectors that involve encryption
  (`[VEC-PROP-ENVELOPE]`, `[VEC-PAPER-URI]`) are proven by a decrypt round trip,
  not by frozen ciphertext.
- LXMF calls `encrypt` for the propagated and paper forms over the message tail
  `packed[16:]` (`LXMessage.py:427,446`), and `Destination.decrypt` on receipt.
- The encryption description strings `"AES-128"` / `"Curve25519"` /
  `"Unencrypted"` (`LXMessage.py:97-99`) are local labels only, not on the wire.

## Key derivation

- **`Cryptography.hkdf(length, derive_from, salt, context)`** is HKDF-SHA-256.
  LXMF uses it only to build the stamp workblock (`LXStamper.py:22-25`); see
  [Stamps and proof-of-work](07-stamps-pow.md).

## Identities

An `Identity` carries an X25519 key pair (encryption) and an Ed25519 key pair
(signing). The reference constructs deterministic identities for the vectors from
fixed 64-byte private material `X25519(32) || Ed25519(32)`
(`gen_vectors.py`, recorded in `vectors.json` `meta.src_identity_prv_hex` /
`meta.dst_identity_prv_hex`). An identity's 16-byte hash is `truncated_hash` of
its concatenated public keys; LXMF treats the hash as opaque and obtains it from
the Reticulum `Destination`.
