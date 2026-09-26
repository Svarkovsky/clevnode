# Cryptographic primitives

Reticulum builds every wire surface on a small set of primitives. An
implementation MUST produce byte-identical results to these, because their
outputs are hashed, signed, and exchanged on the wire.

## Hashing

- **`full_hash(x)`** is SHA-256 over `x`, 32 bytes (`Identity.HASHLENGTH = 256`
  bits, `Identity.py:80,373`).
- **`truncated_hash(x)`** is the leading 16 bytes of `full_hash(x)`
  (`TRUNCATED_HASHLENGTH = 128` bits, `Identity.py:84,383`).

Proven by `[VEC-HASH]`: `full_hash("reticulum-spec") =
659fe249468c635cdfe90a12624abec49f0bd36ba66d467b4f7155c79e8addf2`, truncated to
`659fe249468c635cdfe90a12624abec4`.

## Key exchange and signing

- **X25519** (`Cryptography/X25519.py`): 32-byte keys; `exchange` is constant-time
  ECDH yielding a 32-byte shared secret. Deterministic for a given key pair.
- **Ed25519** (`Cryptography/Ed25519.py`): 32-byte seed, 32-byte public key,
  64-byte signature; deterministic per RFC 8032. `sign(m)` and `verify(sig, m)`.

## Symmetric encryption

- **AES-128/256-CBC** (`Cryptography/AES.py`) with a 16-byte IV and PKCS7 padding
  (`Cryptography/PKCS7.py`, block size 16). `[VEC-AES]`: `AES-256-CBC` of one
  block `"0123456789abcdef"` under key `00010203…1f` and IV `000102…0f` is
  `e23fc0b91c7bd64425c559736e9b0c58`, and decrypts back.

## HMAC and HKDF

- **HMAC-SHA256** (`Cryptography/HMAC.py`, RFC 2104), 32-byte digest.
- **HKDF-SHA256** (`Cryptography/HKDF.py:35`), `hkdf(length, derive_from, salt,
  context)`. `[VEC-HKDF]`: `hkdf(32, derive_from=00..1f, salt=00..0f) =
  2bc3faec9f360e81e77086b6e17a9ce8722a4cb3bc0ed90b4d78d37036e43a0f`.
  The block counter is taken modulo 256 (`Cryptography/HKDF.py:58`), so unlike
  RFC 5869 there is no 255-block ceiling: outputs beyond 8160 bytes keep
  deriving, with the counter wrapping to 0 on block 256. IFAC needs this — its
  mask is as long as the packet.

## Encryption token (modified Fernet)

The token (`Cryptography/Token.py`) is the AEAD-like envelope Reticulum uses for
SINGLE-destination and link encryption. Its layout is:

```
IV(16) || AES-CBC(plaintext, derived_key, IV) || HMAC-SHA256(...)(32)
```

with `TOKEN_OVERHEAD = 48` (IV 16 + HMAC 32, `Token.py:50`). The HMAC
authenticates the IV and ciphertext; `decrypt` MUST verify it before decrypting
(`Token.py:77,100`). The full ECIES wrapper that prepends the ephemeral public
key is specified in [Identity](02-identity.md) and proven by `[VEC-ID-TOKEN]`.

## Determinism

Hashing, HKDF, HMAC, Ed25519 signing, X25519 (given the keys), and AES-CBC (given
key and IV) are deterministic and yield **frozen** vectors. Anything that
generates an ephemeral key or IV (the encryption token, announces, link
handshake) is non-deterministic in normal operation; this specification freezes
those by injecting fixed randomness in the harness and additionally proves them
by roundtrip (see [Test vectors](13-test-vectors.md)).
