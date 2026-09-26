# Announce application data

LXMF carries application data in Reticulum announces. There are two formats: the
delivery announce (sent by a normal LXMF destination) and the propagation-node
announce. Both are normative and proven by `[VEC-ANN-DELIVERY]` and
`[VEC-ANN-PROPAGATION]`.

## Delivery announce

The delivery announce app_data is (`LXMRouter.py:1034-1050`):

```
msgpack([ display_name, stamp_cost, supported_functionality ])
```

- `display_name`: the UTF-8 encoded display name as `bin`, or `None`
  (`LXMRouter.py:1038-1040`).
- `stamp_cost`: an integer in `(0, 255)`, or `None`
  (`LXMRouter.py:1042-1045`).
- `supported_functionality`: a list of advertised feature codes. LXMF 1.0.1
  emits `[SF_COMPRESSION]`, where `SF_COMPRESSION = 0x00`
  (`LXMRouter.py:1047-1048`; `LXMF.py:140-142`).

### Format detection

The decoders distinguish this version-0.5.0+ format from the legacy format
(a bare UTF-8 display name) by sniffing the first byte: it is the new format iff
`app_data[0]` is in `0x90..0x9f` (msgpack fixarray) or equals `0xdc` (array16)
(`LXMF.py:151-200`). An implementation MUST emit a msgpack array so this sniff
succeeds; the current three-element array begins with `0x93`.

For compatibility with earlier senders, the reference compression decoder
treats a missing or non-list third element as compression support. When the
third element is a list, compression is supported only if that list contains
`SF_COMPRESSION` (`LXMF.py:187-200`).

### Proof: `[VEC-ANN-DELIVERY]`

`msgpack([b"Alice", 8, [SF_COMPRESSION]])` produces
`93c405416c696365089100`, with `first_byte = 0x93`. The genuine decoders recover
`display_name = "Alice"`, `stamp_cost = 8`, and compression support
(`LXMF.py:151-200`).

## Propagation-node announce

The propagation announce app_data is a 7-element list
(`LXMRouter.py:328-336`):

```
msgpack([
  legacy_flag,              # 0: bool, legacy LXMF PN support
  timebase,                 # 1: int, int(time.time())
  propagation_enabled,      # 2: bool
  per_transfer_limit_kb,    # 3: int
  per_sync_limit_kb,        # 4: int
  [prop_cost, prop_flex, peering_cost],   # 5: list of three ints
  metadata,                 # 6: dict (PN_META_* keys)
])
```

### Validity

`pn_announce_data_is_valid` (`LXMF.py:224-250`) requires: `data` decodes to a
list of length `>= 7`; `data[1]` (timebase), `data[3]`, `data[4]` are integer-
coercible; `data[2]` is strictly `True` or `False`; `data[5]` is a list whose
first three elements are integer-coercible; and `data[6]` is a dict. An
implementation MUST satisfy all of these for its propagation announce to be
accepted.

### Metadata map (`LXMF.py:128-138`)

Keys: `PN_META_VERSION` (0x00), `PN_META_NAME` (0x01), `PN_META_SYNC_STRATUM`
(0x02), `PN_META_SYNC_THROTTLE` (0x03), `PN_META_AUTH_BAND` (0x04),
`PN_META_UTIL_PRESSURE` (0x05), `PN_META_CUSTOM` (0xFF). The node name is
`metadata[PN_META_NAME]` as UTF-8 bytes (`LXMRouter.py:321`).

### Proof: `[VEC-ANN-PROPAGATION]`

The vector builds the 7-element list with `metadata = {PN_META_NAME: b"Node"}`,
`stamp_costs = [16, 3, 18]`, and a fixed timebase, then proves with the genuine
helpers: `pn_announce_data_is_valid = true`, `pn_name_from_app_data = "Node"`
(`LXMF.py:202-213`), and `pn_stamp_cost_from_app_data = 16`
(`LXMF.py:215-222`).
Field 1 (timebase) is `int(time.time())` in the real protocol and is pinned to a
constant in the vector.

`leviculum-lxmf` decodes this announce to discover a propagation node. Encoding
is documented here as a wire-format requirement; the Rust crate does not host a
propagation node or emit propagation-node announces.
