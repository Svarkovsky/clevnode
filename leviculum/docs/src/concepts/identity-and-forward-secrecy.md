# Cryptographic Identity and Forward Secrecy

Every node and every endpoint in a Reticulum network is identified by
cryptography, not by an address handed out by infrastructure. This
page explains the conceptual model. For the exact byte layouts, defer
to the [Reticulum specification](../appendix/reticulum-specification.md)
(its Identity, Destination, and Announce sections).

## Identities are dual keypairs

A Reticulum identity holds **two** keypairs, used for two different
jobs (`leviculum-core/src/identity.rs:45`):

- **X25519** — for key agreement (ECDH). This is how two parties
  derive a shared secret to encrypt traffic to each other.
- **Ed25519** — for digital signatures. This is how a node proves an
  announce or a packet genuinely came from the holder of the identity.

An identity may be *full* (it holds the private halves and can decrypt
and sign) or *public-only* (it holds just the public keys, learned
from someone else's announce, and can only encrypt and verify). In the
source this is the difference between the `Option`-wrapped private
fields and the always-present public fields
(`leviculum-core/src/identity.rs:48`).

## Destinations are derived addresses

You do not pick a Reticulum address; you *derive* one. A
[Destination](../appendix/reticulum-specification.md) is an addressable
endpoint whose 16-byte hash is computed from an application name, a
set of aspects, and (for most types) an identity
(`leviculum-core/src/destination.rs:1`). Because the address is a hash
of stable inputs, it is reproducible and self-authenticating: anyone
who knows the inputs computes the same address, and the identity bound
into it proves ownership.

A destination also carries a *type* (`SINGLE`, `GROUP`, `PLAIN`,
`LINK`) that selects its encryption behaviour, and a *direction*
(`IN`, `OUT`) that selects whether it can receive or send
(`leviculum-core/src/destination.rs:6-7`).

## Announces carry the public keys

A node makes itself reachable by broadcasting an **announce**: a signed
notification that carries the destination's public keys out into the
mesh. Peers that receive it learn the destination's address *and* the
keys needed to encrypt to it, and Transport learns a path back. The
exact announce wire format is specified in the
[Reticulum spec](../appendix/reticulum-specification.md).

## Ratchets: forward secrecy without a link

End-to-end encryption protects traffic in flight, but if a long-lived
identity key is ever compromised, an attacker who recorded past
ciphertext could decrypt it. **Ratchets** close that window for
packets sent to `SINGLE` destinations *without* first establishing a
Link (`leviculum-core/src/ratchet.rs:1`).

The mechanism, conceptually:

1. A destination enables ratchets and generates an initial X25519
   keypair.
2. It includes the current ratchet public key in its announces.
3. Senders encrypt to the **ratchet** public key, not the long-term
   identity key.
4. The destination rotates its ratchet keypair periodically (default
   ~30 minutes).
5. Old ratchets are retained for a while so late-arriving packets
   still decrypt (default 512 retained), then discarded.

Because the rotating key is short-lived and the private half is thrown
away after rotation, compromising the long-term identity does not
expose traffic encrypted to expired ratchets. That is forward secrecy.

Persisting ratchet keys across restarts is the job of the
[`Storage`](storage-and-embedding.md) trait (the `ratchets/` and
`ratchetkeys/` collections, see
[Architecture](../architecture.md#filestorage-persistence)). Links —
the other path to forward secrecy, via an ephemeral session handshake —
are a separate mechanism; see the
[Reticulum specification](../appendix/reticulum-specification.md).

## Where to read the exact bytes

This page stays conceptual on purpose. The authoritative definitions
of identity serialisation, destination hashing, announce structure,
and ratchet encoding are in the
[Reticulum specification](../appendix/reticulum-specification.md). The
Rust types above (`identity.rs`, `destination.rs`, `ratchet.rs`)
implement that specification.
