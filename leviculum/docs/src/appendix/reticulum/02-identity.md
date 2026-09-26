# Identity

An identity is a pair of key pairs: X25519 for encryption and Ed25519 for
signing. This section is proven by `[VEC-ID-HASH]`, `[VEC-ID-SIGN]`, and
`[VEC-ID-TOKEN]`.

## Key material

The public key is the concatenation (`Identity.py:757`):

```
public_key = X25519_public(32) || Ed25519_public(32)        # 64 bytes
```

and the private key is `X25519_private(32) || Ed25519_seed(32)`
(`Identity.py:750,768-777`). `KEYSIZE = 512` bits (`Identity.py:59`),
`SIGLENGTH = 512` bits (`:81`). `[VEC-ID-HASH]` records a 64-byte public key from
the fixed private material `00010203…3f`.

## Identity hash

```
identity_hash = truncated_hash(X25519_public || Ed25519_public)      # 16 bytes
```

(`Identity.py:805-810`). `[VEC-ID-HASH]`: identity hash `aca31af0441d81dbec71e82da0b4b5f5`.

## Name hash

`NAME_HASH_LENGTH = 80` bits (`Identity.py:83`); the name hash is the leading 10
bytes of `full_hash` of the dotted destination name. Used in destination hashing
and announces; see [Destination](03-destination.md).

## Signing

- `sign(m)` is Ed25519 over `m`, 64 bytes, deterministic (`Identity.py:931-941`).
- `validate(sig, m)` verifies it (`Identity.py:948-964`).

`[VEC-ID-SIGN]`: signing the fixed message yields signature
`bbfdcde5aa05197f…` and `validate` returns true.

## Encryption token (ECIES)

`encrypt(plaintext)` to a SINGLE destination produces (`Identity.py:827-857`):

```
ephemeral_X25519_public(32) || token(IV(16) || AES-CBC ciphertext || HMAC(32))
```

The derivation:

1. generate an ephemeral X25519 key pair (`Identity.py:836`);
2. `shared = ephemeral_private.exchange(target_X25519_public)` (`:844`);
3. `derived_key = hkdf(length=64, derive_from=shared, salt=target_identity_hash,
   context=None)` (`:846-851`);
4. `token = Token(derived_key).encrypt(plaintext)` (`:854`).

`DERIVED_KEY_LENGTH = 64` bytes (`Identity.py:90`): 32 for the AES-256 key and 32
for the HMAC key. `decrypt` recovers the ephemeral public key from the first 32
bytes, re-derives the key, and (when ratchets are present) tries each ratchet
before the base identity (`Identity.py:872-928`).

Because the ephemeral key and IV are random, the token is not reproducible in
normal operation. `[VEC-ID-TOKEN]` is a **frozen-injection** vector: under pinned
randomness the 112-byte token (32 ephemeral + 48 overhead + 32 ciphertext for a
27-byte plaintext) reproduces byte for byte, and `decrypt(token) == plaintext`
holds. An implementation MUST reproduce this construction so a Python peer can
decrypt its packets.

## Ratchets

A ratchet is an ephemeral X25519 key offering forward secrecy. `RATCHETSIZE =
256` bits (`Identity.py:64`); the ratchet id is the leading 10 bytes of
`full_hash` of the ratchet public key (`_get_ratchet_id`, `Identity.py:417`). A
destination MAY advertise its current ratchet in an announce (see
[Announce](05-announce.md)); a sender then encrypts to the ratchet public key
instead of the identity's static key. Ratchet rotation and expiry
(`RATCHET_EXPIRY`, `RATCHET_INTERVAL`) are informative.
