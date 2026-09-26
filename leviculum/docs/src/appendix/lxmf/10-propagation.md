# Propagation

Propagation lets a sender deposit a message at an always-reachable node for an
offline recipient to collect later. The Rust implementation covers both client
directions: origin uploads and the recipient `/get` list, download, and
acknowledgement exchange. `/offer`, node ingest, the node-internal store, peer
selection, rotation, and sync scheduling are Python-reference documentation
only and are not implemented by `leviculum-lxmf`.

## Propagation transfer envelope

A propagated message is wrapped as follows (`LXMessage.py:423-433`):

```
pn_encrypted_data = destination.encrypt(packed[16:])
lxmf_data         = packed[:16] || pn_encrypted_data
transient_id      = full_hash(lxmf_data)
if propagation_stamp: lxmf_data || = propagation_stamp     # 32 bytes
propagation_packed = msgpack([ wall_clock_timestamp, [ lxmf_data ] ])
```

Normative points:

- The destination hash (`packed[:16]`) stays in cleartext; the rest of the packed
  message (`packed[16:]`, i.e. source hash, signature, payload) is encrypted to
  the recipient (`LXMessage.py:427,430`).
- `transient_id = full_hash(lxmf_data)` and is computed **before** any
  propagation stamp is appended (`LXMessage.py:431-432`).
- The envelope is `msgpack([timestamp, [lxmf_data, ...]])`: a timestamp followed
  by a list of one or more `lxmf_data` blobs (`LXMessage.py:433`). The peer-sync
  path reuses the same shape with many blobs (`LXMPeer.py:462`).

## Origin upload (implemented)

An originating client sends a singleton envelope directly as Link data. If the
encoded envelope is at most 319 bytes it is a raw Link Packet; otherwise it is
a Resource (`LXMessage.py:423-441,483-496,608-614`). This is not a request and
does not use `/offer`.

Before upload, the client appends a separate 32-byte propagation-node stamp to
`lxmf_data`. Its work material is the pre-stamp `transient_id`, its target is
the full cost advertised by the selected node, and its workblock uses
`WORKBLOCK_EXPAND_ROUNDS_PN = 1000`. A 16-byte delivery-ticket stamp, when one
is available for the recipient, remains inside the signed clear message before
recipient encryption; it never replaces the outer propagation stamp.

The client serialises one upload per propagation Link. Packet proof or
sender-side Resource completion changes the message to `SENT`. Packet timeout,
Resource failure, or Link closure returns it to the bounded retry queue; Packet
and Resource failures tear down the Link before retry, matching Python. Link
establishment is charged as the logical attempt while submission on that Link
is not charged again. The node signal `msgpack([0xF5])` rejects the message.
Ciphertext, transient ID, envelope timebase, advertised target cost, and
generated outer stamp are checkpointed so restoration and retries do not
re-encrypt or change the bytes. Process-local Link state and monotonic
deadlines are not reusable after a restart; restored entries become
immediately due while retaining their durable retry count and prepared bytes.

The router emits an owned `PropagationStampRequest`; its calculation borrows
neither the router nor `NodeCore`. The default PoW executor yields
cooperatively while expanding and searching the workblock, so Link and receive
events remain serviceable on a single-threaded runtime. Applications can
supply another `StampExecutor` (for example a Rayon-backed pool); builds
without the `pow` feature can attach a detached 32-byte stamp through the
router API.

### Proof: `[VEC-PROP-ENVELOPE]`

Because `destination.encrypt` uses a fresh ephemeral key per call, the ciphertext
is not reproducible; the vector is a round-trip proof. It records deterministic
lengths and the cleartext `dest_hash_prefix`, checks the structure
`destination_hash(16) || destination.encrypt(packed[16:])`, checks that the
derived transient ID is `full_hash(lxmf_data)`, and proves
`destination.decrypt(pn_encrypted) == packed[16:]`
(`decrypt_recovers_inner_tail = true`). The random ciphertext and transient-ID
bytes are deliberately omitted so the canonical fixture remains byte-stable.
An implementation MUST reproduce the framing and transient-ID derivation.

## Propagation-node announce

See [Announce application data](09-announce-appdata.md) for the 7-element node
announce that advertises the node's limits and stamp costs.

## `/offer` (Python propagation peers only; not implemented)

A Python propagation peer requests `OFFER_REQUEST_PATH = "/offer"`
(`LXMPeer.py:14`) over a
Link with the payload (`LXMPeer.py:381,385`):

```
offer = [ peering_key, [ transient_id, ... ] ]
```

where `peering_key` is the party's proof-of-work peering key (see below) and the
list is the transient-ids it offers. The node replies via `offer_response`
(`LXMPeer.py:400`); the reply is one of: `False` (node already has all),
`True` (node wants all), or a list (the subset the node wants). The wanted
messages are then pushed as one Resource carrying
`msgpack([timestamp, [lxmf_data, ...]])` (`LXMPeer.py:462-464`).

`leviculum-lxmf` exposes no `/offer` request path, handler, peering-key engine,
or peer-sync state machine.

## `/get` (collect from a node)

A recipient requests `MESSAGE_GET_PATH = "/get"` (`LXMPeer.py:15`) with the
payload (`LXMRouter.py:1427-1449`):

```
[ want, have ]
```

- if both `want` and `have` are `None`, the node returns a list of the recipient's
  available `transient_id`s, sorted by size (`LXMRouter.py:1436-1449`);
- otherwise `have` lists transient-ids the client already holds (so the node can
  drop them) and `want` lists the ones to send (`LXMRouter.py:1450-1500`).

The exact list, download, acknowledgement, list-response, and download-response
bytes are pinned by `VEC-PROP-GET-LIST`, `VEC-PROP-GET-DOWNLOAD`,
`VEC-PROP-GET-ACK`, `VEC-PROP-LIST-RESPONSE`, and
`VEC-PROP-GET-RESPONSE` in the [test vectors](14-test-vectors.md).

### Current Rust Resource boundary

Python passes the request timeout into an oversized `/get` request Resource,
then starts a fresh response deadline after the upload proof. The Rust client
matches those two timeout phases and the canonical request (`q/u`) and response
(`q/p`) advertisement flags.

The current Core correlation path represents a request or response Resource as
one semantic transfer. The fully packed Link request or response must currently
fit one Reticulum Resource segment: at most `RESOURCE_MAX_EFFICIENT_SIZE` =
1,048,575 bytes. An oversized outgoing request returns `ResourceTooLarge`
before LXMF stores its correlation, and an incoming split request/response
advertisement is ignored. Generic LXMF message Resources still use Core's
ordinary multi-segment transfer path; this limit applies specifically to Link
request/response Resources.

The router's default `/get` delivery transfer limit is 1000 KB (1,024,000
bytes), below the single-segment ceiling. Full Python-compatible semantic
reassembly for larger request/response Resources remains deferred.

## Error codes (`LXMPeer.py:24-31`)

Returned by node request handlers or, for `ERROR_INVALID_STAMP`, as upload Link
signalling:

| Code | Name | Meaning |
|------|------|---------|
| 0xF0 | `ERROR_NO_IDENTITY` | requester did not identify on the link |
| 0xF1 | `ERROR_NO_ACCESS` | requester not allowed |
| 0xF3 | `ERROR_INVALID_KEY` | invalid peering key |
| 0xF4 | `ERROR_INVALID_DATA` | malformed request |
| 0xF5 | `ERROR_INVALID_STAMP` | propagation stamp invalid |
| 0xF6 | `ERROR_THROTTLED` | rate limited (`PN_STAMP_THROTTLE` = 180 s) |
| 0xFD | `ERROR_NOT_FOUND` | requested message not found |
| 0xFE | `ERROR_TIMEOUT` | request timed out |

## Peering key (Python reference only; not implemented)

A peering key is a proof-of-work over `peer_identity_hash || node_identity_hash`
with `WORKBLOCK_EXPAND_ROUNDS_PEERING` = 25 rounds against the node's advertised
`peering_cost` (`LXMPeer.py:242-265`; validated by `validate_peering_key`,
`LXStamper.py:79-82`). It authorizes a party to offer messages to the node.

## Node transient ingest and expiry (Python reference only; not implemented)

A node stores each accepted message keyed by `transient_id`, validates its
propagation stamp in batches (`LXStamper.py:87-90`), and expires entries after
`MESSAGE_EXPIRY` = 30 days (`LXMRouter.py:38`). The on-disk layout and peer
bookkeeping are informative.
