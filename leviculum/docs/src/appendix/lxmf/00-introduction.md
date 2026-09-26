# Introduction and scope

## Reference

The active wire fixture and Rust compatibility tests use these pinned
references:

| Component | Version | Commit |
|-----------|---------|--------|
| LXMF | 1.1.0 (`_version.py:1`) | `795fdaa2b0777c13033787d933d1afc94a2377cb` |
| Reticulum (RNS) | 1.3.5 | `d5e62d4e15c5fe2e170f7bd9e120551671f21a27` |

`APP_NAME` is `"lxmf"` (`LXMF.py:1`). Where the reference defers to a Reticulum
primitive (hashing, signing, encryption, MDU sizes), this document cites
`reference/Reticulum` and does not re-specify the primitive; its behaviour-as-used
is pinned by test vectors instead.

The frozen symbol inventory and original source-line audit were captured from
LXMF 0.9.6 (`8499729`). The canonical fixture is now generated from 1.1.0, and
sections changed since the audit carry current citations and vectors.

## Scope

**Normative** (this document specifies exactly, and proves):

- the message binary layout, hashing input, signing input, and verification;
- the message payload msgpack structure and its type discipline;
- the fields dictionary and its identifiers;
- delivery method selection and the size thresholds that drive it;
- the on-air form of each delivery method (opportunistic, direct, propagated,
  paper);
- stamp construction, validity, value, and the ticket shortcut;
- announce application-data formats (delivery and propagation);
- the recipient-facing propagation `/get` exchange and its error codes;
- the Python reference's `/offer` and propagation-ingest wire shapes, for
  protocol documentation only.

**Informative** (described, not byte-proven; an implementation MAY diverge):

- router job scheduling, outbound/inbound queue management, retry cadences and
  timeouts;
- on-disk persistence layout (message store, peers, tickets, costs, stats);
- propagation-node peer selection, rotation, and sync scheduling internals.

**Out of scope**: the `lxmd` daemon and CLI (`Utilities/lxmd.py`).

## Rust implementation scope

`leviculum-lxmf` implements propagation only as a client: discovery, outgoing
link establishment, origin uploads as raw Link Packets or Resources, `/get`
list/download requests, and acknowledgement/purge. It does not implement
propagation-node hosting, transit storage, `/offer`, propagation peers, peer
rotation, or peer synchronisation. `/get` requests and responses are currently
bounded to one Reticulum Resource segment; Python-compatible splitting above
that boundary is deferred. References to the unimplemented mechanisms below
describe the Python protocol and do not imply a Rust server surface.

The historical enumeration of reference symbols and their normative /
informative / out-of-scope classification is the frozen
[Symbol inventory](_inventory.md). The
[Coverage ledger](13-coverage-ledger.md) maps every normative symbol to a
section and a proof; a normative symbol with no mapping is a coverage gap.

## Notational conventions

- **RFC 2119 keywords** (MUST, MUST NOT, SHOULD, MAY) carry their usual meaning
  and mark normative requirements.
- **Citations** take the form `(LXMessage.py:364)` and refer to the pinned
  reference file under `reference/LXMF/LXMF/` unless another path is given.
- **Byte layouts** are shown as offset tables or annotated hex. Concatenation is
  written `a || b`. A field width in bytes is shown as `name(16)`.
- **Test vectors** are referenced by label, e.g. `[VEC-MSG-1]`, and are listed
  in full in [Test vectors](14-test-vectors.md). They live in machine-readable
  form in [`vectors/vectors.json`](vectors/vectors.json).
- **Hashes** are SHA-256 unless stated. Integers in stamp arithmetic are
  big-endian (`LXStamper.py:35,45`).

## Regenerating the vectors

From the repository root:

```
PYTHONPATH=reference/Reticulum:reference/LXMF \
    python3 docs/src/appendix/lxmf/vectors/gen_vectors.py
```

The harness fixes all identity key material and the message timestamp, runs the
genuine reference code, asserts determinism for every frozen vector, and writes
`vectors/vectors.json` with the submodule commits recorded in its `meta` block.
Re-running MUST reproduce the committed file byte for byte. Vectors whose output
depends on ephemeral encryption key material are marked `roundtrip` and are
proven by a decrypt round trip plus structural assertions rather than by frozen
ciphertext.
