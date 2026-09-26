# Message binary format

This section is normative and is proven by `[VEC-MSG-1]`, `[VEC-MSG-2]`, and
`[VEC-MSG-3]`.

## Packed layout

A packed LXMF message is the concatenation (`LXMessage.py:379-383`):

```
destination_hash(16) || source_hash(16) || signature(64) || packed_payload
offset 0              16                 32                96
```

An implementation MUST produce exactly this layout. `packed_payload` is the
msgpack serialization of the payload array (below). The total fixed prefix is 96
bytes.

## Payload array

The payload is a msgpack array (`LXMessage.py:359`):

```
[ timestamp, title, content, fields ]
```

with an optional fifth element `stamp` appended when a stamp is generated
(`LXMessage.py:368-370`); see [Stamps](07-stamps-pow.md).

The msgpack **type discipline is normative for a writer** and is a common
interop trap. It is deliberately *not* normative for a reader: see
[Reader tolerance](#reader-tolerance) below, which is the other half of the
same trap.

| Element | msgpack type | Citation |
|---------|--------------|----------|
| `timestamp` | float64 (`f64`), seconds since the Unix epoch | `LXMessage.py:354,359` |
| `title` | binary (`bin`), not string | `LXMessage.py:196-197` |
| `content` | binary (`bin`), not string | `LXMessage.py:199-202` |
| `fields` | map, integer keys (may be empty `{}`) | `LXMessage.py:212-216` |
| `stamp` (optional) | binary (`bin`), 32 bytes | `LXMessage.py:370` |

`title` and `content` are stored and packed as bytes; the `*_as_string`
accessors only decode UTF-8 on demand (`LXMessage.py:196,205`). An implementation
MUST pack them as msgpack `bin`, never `str`. Mismatching this changes the
serialized bytes and therefore the message-id, so a Python peer rejects the
message.

### Proof: annotated `[VEC-MSG-1]` payload

For `timestamp = 1700000000.0`, `title = b"Hi"`, `content = b"Hello"`,
`fields = {}`, the packed payload is `94cb41d954fc40000000c4024869c40548656c6c6f80`:

```
94                     fixarray, 4 elements
cb 41d954fc40000000    float64  = 1700000000.0      (timestamp)
c4 02 4869             bin8 len 2  = "Hi"            (title)
c4 05 48656c6c6f       bin8 len 5  = "Hello"         (content)
80                     fixmap, 0 entries = {}        (fields)
```

The `cb` (float64), `c4` (bin8), and `80` (fixmap) prefixes prove the type
discipline directly. `[VEC-MSG-2]` shows a non-empty `fields` map carrying an
integer key.

## Reader tolerance

A reader MUST NOT require the writer's type discipline for `timestamp`. The
reference reads `timestamp = unpacked_payload[0]` (`LXMessage.py:765`) with no
type check, so **any msgpack number** — every unsigned and signed integer
width, float32, float64 — is accepted and reaches the application. Measured on
the pinned reference: `uint32`, `uint64`, positive and negative fixints,
`int8`..`int64`, `float32`, and the non-finite float64s all unpack with
`signature_validated = true`. So do `nil`, booleans and strings, which no
consumer can use as a time; a reader that carries the timestamp in a numeric
type MAY refuse those, and SHOULD refuse them by a distinguishable error.

LXMF's own writer never exercises this: `time.time()` is a Python float, which
umsgpack always packs as float64. The tolerance matters for the third-party
writers in a real mesh. `[VEC-MSG-FOREIGN-UINT32]`,
`[VEC-MSG-FOREIGN-FLOAT32]` and `[VEC-MSG-FOREIGN-NEGATIVE-FIXINT]` record the
reference decoder's verdict on each form.

`title` and `content` are a different case: the reference does not type-check
them either, but `set_title_from_bytes` (`LXMessage.py:196-197`) stores
whatever it is handed, and a `str`-typed title produces a message whose bytes
no writer that follows this specification would have produced. A reader MAY
require `bin` for those.

## Hashing input (message-id)

The message hash is (`LXMessage.py:361-366`):

```
hashed_part = destination_hash || source_hash || msgpack(payload_without_stamp)
message_id  = full_hash(hashed_part)
```

The payload hashed here MUST NOT include the optional stamp element.
`[VEC-MSG-1]` records `hashed_part_hex` and the resulting `message_id_hex`.

On unpack the reference takes the hashed payload from one of two places, and
the difference is normative (`LXMessage.py:751-762`):

- **No stamp** (`len(unpacked_payload) == 4`): `packed_payload` is the slice
  taken from the wire, hashed **verbatim**. A reader MUST NOT re-encode it. Any
  encoding the writer chose therefore verifies, which is what makes the reader
  tolerance above usable rather than decorative.
- **Stamp present**: the reference discards the received bytes and hashes
  `msgpack.packb(unpacked_payload[:4])` (`:758`) — the decoded values packed
  again by the reader's own encoder. A writer whose encoding is not what
  `msgpack.packb` would emit therefore fails verification on the stamped path
  even though it passes on the unstamped one. Measured: a uint32-encoded
  timestamp with a stamp verifies (Python re-packs it as uint32), the same
  value spelled as uint64 does not, and neither does float32.

The asymmetry is the reference's, not a specification choice, and an
implementation MUST reproduce both branches or it will compute a different
message-id than the sender for some legal inputs.

## Signing input

The signature is (`LXMessage.py:372-375`):

```
signed_part = hashed_part || message_id        (= dest || src || msgpack(payload) || message_id)
signature   = source.sign(signed_part)         (Ed25519, 64 bytes)
```

`[VEC-MSG-1]` records `signed_part_hex`, `signature_hex`, and
`signature_valid = true` (verified with `source.identity.validate`).

## Unpack and verification

`unpack_from_bytes` (`LXMessage.py:735-807`) slices at the fixed offsets:
`destination_hash = bytes[0:16]`, `source_hash = bytes[16:32]`,
`signature = bytes[32:96]`, `packed_payload = bytes[96:]`. It unpacks the
payload, and if the array has more than four elements treats element `[4]` as the
stamp and removes it before recomputing the hash (`LXMessage.py:754-758`); see
[Hashing input](#hashing-input-message-id) for which bytes are hashed in each
case.

The reference also caches the received bytes (`message.packed = lxmf_bytes`,
`LXMessage.py:799`) and `pack()` returns them untouched when they are present
(`if not self.packed`, `:355`). An implementation that re-serialises an
unpacked message instead of returning what it received will emit bytes the
message's own signature does not cover.

Verification requires the source identity to be known (learned from its
announce). The outcome is one of (`LXMessage.py:790-801`):

- signature valid: `signature_validated = true`;
- signature present but invalid: `unverified_reason = SIGNATURE_INVALID (0x02)`;
- source identity unknown: `unverified_reason = SOURCE_UNKNOWN (0x01)`.

`[VEC-MSG-3]` makes the source identity recallable, unpacks `[VEC-MSG-1]`, and
records `signature_validated = true`, `matches_source = true`, and the recovered
title and content. An implementation MUST reproduce these offsets and the
stamp-stripping rule, or it will compute a different message-id than the sender.
