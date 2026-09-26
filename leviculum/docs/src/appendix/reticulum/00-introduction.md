# Introduction and scope

## Reference

This specification describes Reticulum as implemented by the pinned reference:

| Component | Version | Commit |
|-----------|---------|--------|
| Reticulum (RNS) | 1.3.5 | `d5e62d4e15c5fe2e170f7bd9e120551671f21a27` |

Citations reference files under `reference/Reticulum/RNS/` unless another path is
given.

## Scope

**Normative** (specified exactly and proven):

- the cryptographic primitives as used (hashes, X25519, Ed25519, AES-CBC, HMAC,
  HKDF, the encryption token);
- identity key material, hashing, signing, and the ECIES encryption token;
- destination naming, hash derivation, and types;
- the packet header bitfield, both header types, addressing, context bytes, and
  sizes;
- announce layout, signing, and validation;
- link establishment (request, proof, link id, session key) and the link
  context-byte payloads;
- resource advertisement, parts, hashmap, requests, and proof;
- channel envelope and stream-data framing;
- the transport path request and path response packets;
- byte-stream framing (HDLC/KISS) and IFAC masking.

**Informative** (described, not byte-proven; an implementation MAY diverge):

- transport routing internals: path/announce/link/reverse/tunnel tables and their
  index shapes, announce retransmission timing and jitter, deduplication, table
  TTLs;
- resource flow-control window adaptation and timeout factors;
- keepalive interval math and link watchdog scheduling.

**Out of scope**: interface drivers beyond framing, the daemon/CLI tooling, and
the shared-instance IPC.

The full enumeration and classification of reference symbols is the frozen
[Symbol inventory](_inventory.md). The [Coverage ledger](12-coverage-ledger.md)
maps every normative symbol to a section and a proof; a normative symbol with no
mapping is a coverage gap.

## Notational conventions

- **RFC 2119 keywords** (MUST, MUST NOT, SHOULD, MAY) mark normative requirements.
- **Citations** take the form `(Packet.py:177)`.
- **Byte layouts** are shown as offset tables or annotated hex; concatenation is
  `a || b`; a field width is `name(16)`.
- **Test vectors** are referenced by label `[VEC-...]` and listed in full in
  [Test vectors](13-test-vectors.md); machine-readable in
  [`vectors/vectors.json`](vectors/vectors.json).
- **Hashes** are SHA-256 unless stated; the *truncated hash* is its leading 16
  bytes (`Identity.py:383`). Integers in masks/derivations are big-endian.

## Vector kinds

- **frozen** — deterministic; the hex is the proof; reproduces byte for byte.
- **frozen-injection** — an ephemeral-key path (token, announce, link handshake,
  path request) made reproducible by pinning `os.urandom`, `time`, and X25519
  ephemeral key generation in the harness, with a decrypt/verify/derive roundtrip
  recorded as the semantic proof.
- **computed** — reconstructed per the source layout where building a live object
  needs a running link or transport (HEADER_2 packet, resource advertisement);
  the construction is byte-exact and cited.

## Regenerating the vectors

From the repository root:

```
PYTHONPATH=reference/Reticulum \
    python3 docs/src/appendix/reticulum/vectors/gen_vectors.py
```

The harness boots a headless `RNS.Reticulum` instance, fixes all key material,
runs the genuine reference code, asserts determinism for every frozen and
frozen-injection vector by rebuilding and comparing, and writes
`vectors/vectors.json` with the submodule commit in its `meta` block. Re-running
MUST reproduce the committed file byte for byte.
