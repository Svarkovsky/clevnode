# LXMF and LXST on libreticulum: Feasibility Study and Crate Design Proposal

Status: historical feasibility study. Original date: 2026-06-22.

> **Implementation update (2026-07-16):** `leviculum-lxmf` now implements the
> LXMF portions of this design as `no_std + alloc`. The §6.3 decisions are
> reflected in the code: MessagePack uses `rmp` without default features and
> is locked to Python golden vectors; ratchet-aware destination encryption is
> exposed through `NodeCore`; Resource compression is enabled; PoW is
> feature-gated with cooperative/custom executors; and the vendored Python
> reference is hash-locked at LXMF 1.1.0 (`795fdaa`) in tests. Propagation
> support is intentionally client-only: origin uploads use the reference raw
> Link Packet/Resource envelope, while recipients use the `/get` list,
> download, and acknowledgement exchange. Propagation-node
> serving, transit storage, `/offer`, and peer synchronisation remain deferred
> as proposed in §3.5 and Stage 5. Link request and response Resources are
> currently limited to one Reticulum Resource segment (at most 1,048,575
> packed bytes); Python's split request/response Resource reassembly remains
> deferred with the propagation-server work.

This report assesses building two new crates on top of `leviculum-core`:
an LXMF crate (messaging) and an LXST crate (real-time media / telephony),
both targeting `no_std + alloc` so they run on embedded targets (nRF52840
and similar) as well as on hosts.

It is grounded in four codebases:

- `reference/LXMF` (markqvist, audited at v0.9.6; active lock v1.1.0) — the
  authoritative Python reference.
- `reference/LXST` (markqvist, v0.4.4) — the authoritative Python reference.
- `/home/lew/coding/rsLXMF` — a third-party Rust LXMF implementation.
- `/home/lew/coding/rsLXST` — a third-party Rust LXST implementation.

Both Python repos are now pinned as submodules under `reference/` exactly like
`reference/Reticulum`.

---

## 1. Executive summary

**Both crates are feasible. They are not equally `no_std`.**

| Crate | `no_std + alloc` verdict | Reason |
|-------|--------------------------|--------|
| **LXMF** | Feasible end to end. The full protocol (framing, signing, delivery method selection, stamps, tickets, even propagation) is pure byte and state logic over RNS primitives our core already exposes. | All of LXMF is "move opaque bytes over Identity/Destination/Packet/Link/Resource". No audio, no DSP, no native libs. |
| **LXST** | Feasible **for the protocol layer only** (signaling state machine + frame container + jitter policy + Null/Raw codecs). Media capture/playback, the Opus/Codec2 DSP, and soundcard I/O are fundamentally **not** `no_std` and stay host-side behind trait objects. | LXST already draws this line itself: a frame is opaque bytes at the network boundary. |

The decisive architectural fact, confirmed against our own core: **`leviculum-core`
is a true sans-IO `#![no_std] + alloc` state machine.** It has no async, no tokio,
no I/O. The application boundary is `NodeCore`: you feed bytes in
(`handle_packet`, `handle_timeout`) and drain `TickOutput { actions, events }`.
Platform concerns are injected via the `Clock` and `Storage` traits and an RNG.
An LXMF/LXST layer can therefore sit directly on `leviculum-core` with no std
runtime — the existing `leviculum-nrf` firmware already drives `NodeCore` this
way without tokio. `leviculum-std`'s tokio `Reticulum`/`ReticulumNode` is a
convenience driver, not a dependency we are forced through.

**On reuse of the existing Rust code:** the two existing crates are built
against a *different*, unrelated Rust Reticulum (`rsReticulum`, crates `rns-*`),
which we will not adopt. They are also std/tokio, not `no_std`. **However**,
both are licensed **AGPL-3.0-or-later — identical to libreticulum** (the audits'
"AGPL blocker" does not apply to us; the vendored `opus-rs` is BSD-3-Clause,
also compatible). So the choice is purely technical, not legal:

- **rsLXST is a genuine adopt-partial.** Its `lxst-core` is deliberately
  RNS-free and audio-free; `wire.rs`, `profile.rs`, `raw.rs`, `telephony.rs`,
  `stream.rs` are clean, well-tested-against-Python protocol logic that ports
  with light edits. Its Python-parity test harness is directly adoptable.
- **rsLXMF is reference-only.** ~70% of its lines are wired to `rsReticulum`'s
  `Link`/`Resource`/`Transport` API or to std/tokio/`SystemTime`/`std::fs`
  baked into the core type. Worse, it ships a **wire-incompatible message-stamp
  PoW** (a "simplified" iterated-SHA256 instead of the Python HKDF-expanded
  workblock). Mine its `constants.rs` and its msgpack layout as a *spec*; do not
  adopt modules as code. The Python `reference/LXMF` is the real authority.

Recommended path: two new workspace crates, `lxmf` and `lxst`, both
`#![no_std] + alloc`, both depending only on `leviculum-core` for protocol
logic, with thin std-side glue (and, for LXST, host media traits) layered on
top. Stage LXMF first (opportunistic + direct delivery), defer propagation-node
*serving* and LXST media to later milestones.

---

## 2. Target platform: what `leviculum-core` gives us

This section is the foundation; everything else is judged against it.

### 2.1 Sans-IO, no_std, no async

- `leviculum-core/src/lib.rs:59` `#![no_std]`, `:70` `extern crate alloc`.
  `Cargo.toml:11` documents "always no_std". No `async`/`.await`/tokio anywhere
  in `core/src`.
- The application boundary is `NodeCore<R: CryptoRngCore, C: Clock, S: Storage>`
  (`node/mod.rs:139`):
  - inbound bytes: `handle_packet(iface, &[u8]) -> TickOutput` (`node/mod.rs:1028`)
  - maintenance tick: `handle_timeout() -> TickOutput` (`node/mod.rs:1120`)
  - scheduling hint: `next_deadline() -> Option<u64>` (`node/mod.rs:1151`)
  - every send method returns `TickOutput` instead of doing I/O.
- `TickOutput { actions: Vec<Action>, events: Vec<NodeEvent> }` (`transport.rs:138`);
  `Action = SendPacket{iface,data} | Broadcast{data,exclude_iface}`
  (`transport.rs:113`) — bytes already framed.
- I/O injected via `Clock` (`traits.rs:162`) and `Storage` (`traits.rs:196`);
  RNG via `&mut impl CryptoRngCore`. Core ships `MemoryStorage` and
  `EmbeddedStorage`; `leviculum-std` ships `SystemClock`.

**Implication:** an LXMF/LXST crate drives `NodeCore` synchronously, dispatches
`TickOutput.actions` to whatever transport the host provides, and consumes
`TickOutput.events`. This is exactly the embedded model and exactly what a
`no_std` protocol layer needs.

### 2.2 Primitives available, per LXMF/LXST need

| Need | Available on core? | Reference |
|------|--------------------|-----------|
| Identity: sign / verify / hash / encrypt / decrypt | Yes, public, no_std | `identity.rs:190,202,155,310,382` |
| Destination: construct, hash, announce, encrypt-to-dest (ratchet-aware) | Yes, public | `destination.rs:285,334,752,674,640` |
| Send single addressed packet | Yes via `NodeCore::send_single_packet` | `node/mod.rs:469` |
| Receive inbound (events, not callbacks) | Yes via `NodeEvent::PacketReceived` etc. | `event.rs:59,24` |
| Announce / learn peers | Yes; `ReceivedAnnounce` exposes keys, app_data, ratchet, to_identity | `announce.rs:263` |
| Link: connect / accept / send / identify / close | Yes via NodeCore orchestration | `link_management.rs:185,276,449`, `node/mod.rs:574` |
| Link inbound (request/established/message/closed) | Yes via events | `event.rs:84,94,105,121` |
| Resource: send / accept / progress / complete | Yes via `NodeCore::send_resource` etc. | `node/mod.rs:852,898` |

### 2.3 Gaps in the core API we will hit

1. **No in-core `Clock` impl** — trivial to supply; `leviculum-std` has `SystemClock`.
2. **No event loop / interface plumbing in core** (by design). The crate writes
   its own `actions` dispatcher and `next_deadline` scheduler, or runs on the
   `leviculum-std` driver on hosts.
3. **Ratchet-aware Identity crypto is `pub(crate)`.** `Identity::encrypt_for_destination`
   (`identity.rs:439`) and `decrypt_with_ratchets` (`:515`) are reachable only
   via `Destination::encrypt/decrypt` or `NodeCore` send methods. LXMF's
   PROPAGATED and PAPER paths need encrypt-to-destination; today that means
   holding a `Destination` value (workable) or we expose a thin public wrapper.
   **Decision point for the core crate, not a blocker.**
4. **Resource engine types are `pub(crate)`.** Fine for LXMF-over-a-Link
   (driven through `NodeCore`); no standalone resource framing for exotic use.
5. **BZ2 compression is `compression` feature, off by default**
   (`core/Cargo.toml:12`). A compressed Resource from a Python peer returns
   `CompressionUnsupported` (`incoming.rs:479`) when the feature is off — a
   **Priority-1 wire-compatibility concern** for large LXMF messages. LXMF must
   require `leviculum-core/compression`. `libbz2-rs-sys` is no_std-friendly.
6. **`remember_identity` precondition.** `send_single_packet` needs the peer
   identity remembered first (`node/mod.rs:369`); LXMF learns it from announces.

None of these is a structural blocker. (3) and (5) are the two that touch
`leviculum-core` itself and should be decided before LXMF coding starts.

---

## 3. LXMF protocol (from `reference/LXMF`, audited at v0.9.6)

`APP_NAME = "lxmf"` (`LXMF.py:1`). All of LXMF is opaque-byte movement over RNS;
nothing in it is hostile to `no_std + alloc` except resource sizing, persistence,
threads, time, and proof-of-work memory — all addressable (see §3.6).

### 3.1 Wire format

Constants (`LXMessage.py:39-62`): `DESTINATION_LENGTH = 16`, `SIGNATURE_LENGTH = 64`
(Ed25519), `TICKET_LENGTH = 16`, `STAMP_SIZE = 32`, `LXMF_OVERHEAD = 112`.

Fully-packed message (`pack()`, `LXMessage.py:352-384`):

```
[ destination_hash : 16 ][ source_hash : 16 ][ signature : 64 ][ packed_payload : msgpack ]
```

`packed_payload = msgpack([timestamp:f64, title:bytes, content:bytes, fields:map])`,
with an optional 5th element `stamp:32` appended when the stamp is not deferred
(`:368-370`). **Title and content are msgpack `bin`, not `str`** — a classic
interop trap.

Hashing and signing (`:361-375`):

```
hashed_part = dest || src || msgpack(payload)        # WITHOUT stamp
message_id  = SHA-256(hashed_part)                   # 32 bytes
signed_part = hashed_part || message_id
signature   = source.sign(signed_part)               # Ed25519
```

On unpack, if the payload has >4 elements, element [4] is the stamp; it is
stripped before re-hashing (`:744-747`, `:792-801`).

Transport-method-dependent forms (`:386-455`):

- **OPPORTUNISTIC:** sends `packed[16:]` as a single Packet; the destination
  hash is inferred from the RNS header (`:631`).
- **DIRECT:** full `packed` over a Link, as Packet (small) or Resource (large).
- **PROPAGATED:** `lxmf_data = packed[:16] || destination.encrypt(packed[16:])`;
  `transient_id = SHA-256(lxmf_data)` (computed *before* any propagation stamp);
  wrapped as `msgpack([timestamp, [lxmf_data, ...]])` (`:423-441`).
- **PAPER:** `packed[:16] || destination.encrypt(packed[16:])`, base64url with
  scheme `lxm://`, padding stripped (`:443-455`, `:687-702`).

Field IDs (`LXMF.py:8-41`): `FIELD_EMBEDDED_LXMS=0x01 … FIELD_TICKET=0x0C …
RENDERER=0x0F`, custom `0xFB-0xFD`, debug `0xFE-0xFF`.

### 3.2 Delivery method selection

Methods (`LXMessage.py:29-33`): `OPPORTUNISTIC=0x01, DIRECT=0x02, PROPAGATED=0x03,
PAPER=0x05`; representations `PACKET=0x01, RESOURCE=0x02`. Default desired = DIRECT.

Size thresholds (`:64-94`): `ENCRYPTED_PACKET_MAX_CONTENT = 295`,
`LINK_PACKET_MAX_CONTENT = 319`, `PLAIN_PACKET_MAX_CONTENT = 368`.
`content_size = len(packed_payload) - 16`. Opportunistic over 295 B falls back to
DIRECT; direct over 319 B becomes a Resource. These thresholds map 1:1 to our
core's single-packet vs Resource decision.

### 3.3 Stamps / proof-of-work (`LXStamper.py`)

Anti-spam PoW bound to message-id (delivery) or transient-id (propagation), plus
peering keys for propagation-node authorization.

- Workblock (`:18-29`): `concat over n in [0..expand_rounds): HKDF(256, material,
  salt=SHA256(material || msgpack(n)))`. Rounds: delivery 3000, PN 1000, peering 25.
- Validity (`:42-46`): `target = 1 << (256 - cost)`; valid iff
  `int(SHA-256(workblock || stamp)) <= target`. Cost = leading-zero-bits required.
- Ticket shortcut (`:296-300`): a held outbound ticket yields
  `stamp = truncated_hash(ticket || message_id)`, `stamp_value = 256`, no PoW.

**`no_std` cost (important):** the delivery workblock is `3000 * 256 ≈ 768 KB` in
RAM, peering only 6.4 KB. Generation is brute-force; validation builds one
workblock + one hash. On an MCU the 768 KB delivery workblock may exceed RAM.
Mitigation: make stamp cost configurable, default 0 on constrained nodes, stream
the workblock, or gate PoW behind a `pow` feature. The **bit-exact big-endian
comparison and leading-zero-bit counting must match Python** for cross-stack
stamp acceptance.

### 3.4 Router responsibilities (`LXMRouter.py`)

Delivery + propagation destinations (`IN/SINGLE/lxmf/delivery`,
`.../propagation`), an outbound queue running the per-method FSM, announce
handling that wakes pending messages and updates stamp cost, the ticket system
(`FIELD_TICKET`, 16-byte secrets, expiry/grace/renew windows), inbound dedup via
`locally_delivered_transient_ids`, and a 4-second jobloop. In our design the
"router" becomes a sans-IO state object the host ticks alongside `NodeCore`.

### 3.5 Propagation (server side, defer to a later milestone)

Propagation nodes advertise a 7-field msgpack announce (`Handlers.py:46-54`),
store one file per message (`transient_id_timestamp_stampvalue`), and run a peer
sync state machine (`LXMPeer.py:17-22`: IDLE → LINK_ESTABLISHING → … →
RESOURCE_TRANSFERRING) over `/offer` and `/get` Link requests with batched
Resources. This is the most server-heavy, persistence-heavy, allocation-heavy
part. A `no_std` *client* (send via a PN, fetch own mail) is realistic; running a
*propagation node* on an MCU is not the near-term goal.

### 3.6 `no_std` hazards and mitigations

| Hazard | Mitigation |
|--------|-----------|
| msgpack everywhere, with exact type discipline (bin vs str, float ts, map) | `no_std` msgpack (`rmp`/`rmpv` with alloc) or a hand-rolled fixed-shape codec; replicate the `0x90/0xdc` announce-format sniff |
| Filesystem persistence (messagestore, peers, tickets, costs) | Abstract behind the existing `Storage` trait; filename-encoded metadata becomes an in-memory or trait-backed index |
| Threads + locks (jobloop, PoW workers, 4 locks) | Cooperative ticking driven by the host; no threads |
| Wall-clock time signed into the wire | Inject via `Clock`; wall-clock is load-bearing (payload[0]) so the host must provide real time |
| Unbounded queues/maps | Bounded capacity + eviction on constrained targets |
| PoW workblock 768 KB | Configurable cost, `pow` feature, streamed workblock; default-0 on MCUs |
| Large Resource/sync buffers | Stream/chunk; rely on core's Resource engine rather than building whole batches in RAM |

---

## 4. LXST protocol (from `reference/LXST`, v0.4.4)

`APP_NAME = "lxst"`. The single most important finding: **LXST already splits
cleanly into a tiny protocol layer and a large media layer, and a frame is
opaque bytes at the boundary.** Our `no_std` crate implements the protocol layer
and exposes the media boundary as host traits.

### 4.1 The split

- **Protocol / wire (no_std-portable):** `Network.py` (frame container +
  packetizer + jitter), `Primitives/Telephony.py` (the live signaling state
  machine), the codec-id byte registry (`Codecs/__init__.py`).
- **Media (NOT no_std):** `Sources.py`, `Sinks.py`, `Mixer.py`, `Filters.py`,
  `Generators.py`, `Pipeline.py`, `Codecs/{Opus,Codec2}.py`, the per-OS
  soundcard backends. Thread-per-component, numpy float32 DSP, native libopus /
  libcodec2, file/Ogg I/O.

### 4.2 Wire format (`Network.py`)

Transport is **bare RNS Link packets, fire-and-forget** —
`RNS.Packet(link, msgpack_bytes, create_receipt=False)` (`Network.py:29,64`).
**No Resource, no Channel, no Buffer is used anywhere** in LXST. The container is
a msgpack map with two integer keys:

```
FIELD_SIGNALLING = 0x00   ->  [ signal, ... ]
FIELD_FRAMES     = 0x01   ->  codec_byte || encoded_frame   (or a list of frames)
```

Codec bytes (`Codecs/__init__.py:8-11`): `RAW=0x00, OPUS=0x01, CODEC2=0x02,
NULL=0xFF`. Runtime codec switching is a feature: the receiver re-instantiates
its decoder when `frame[0]` changes (`Network.py:120-123`).

**No application sequence numbers, no timestamps on the wire.** Ordering and loss
are absorbed entirely by the receive **jitter buffer** in the sink
(`Sinks.py:118-208`: a `deque(maxlen=6)`, autostart at 1 frame, drop-oldest on
overflow, underrun timeout). This jitter policy is pure ring-buffer logic and is
`no_std`-portable with `heapless::Deque`.

### 4.3 Telephony signaling (`Primitives/Telephony.py`)

A call **is an RNS Link** to `IN/SINGLE/lxst/telephony` (`Telephony.py:141`),
`PROVE_NONE`. Signaling codes carried in the `FIELD_SIGNALLING` list
(`:102-112`): `STATUS_BUSY=0x00, REJECTED=0x01, CALLING=0x02, AVAILABLE=0x03,
RINGING=0x04, CONNECTING=0x05, ESTABLISHED=0x06, PREFERRED_PROFILE=0xFF`. Profile
negotiation overloads the same channel as `0xFF + profile_code` — **an int >255,
so the signal channel must not be typed `u8`** (model as a variable-width int).

Handshake: caller opens Link → callee sends AVAILABLE → caller `link.identify` →
callee allow/block check, RINGING + ringtone → caller sends preferred profile,
dial tone → callee `answer()` sends CONNECTING, builds pipelines, then
ESTABLISHED → media flows. Timeouts: `RING_TIME=60s`, `CONNECT_TIME=5s`,
`WAIT_TIME=70s`. The state machine is pure event-in / action-out logic over Link
callbacks — directly portable. `Call.py` is a legacy stub; ignore it.

### 4.4 Codecs

| Codec | Pure-Rust portable? | Native dep |
|-------|---------------------|-----------|
| Null (0xFF) | Yes (identity passthrough) | none |
| Raw (0x00) | Yes (1 header byte `(bitdepth<<6)|(channels-1)`, then samples) | numpy only |
| Opus (0x01) | No | libopus (pyogg) |
| Codec2 (0x02) | No | libcodec2 (pycodec2) |

The codec-id registry and per-codec sub-header parsing are `no_std`-portable even
when the DSP is delegated. Null + Raw ship in the `no_std` crate; Opus/Codec2 are
host-side. (Note: rsLXST vendored a *pure-Rust* `opus-rs`, so a std-side Opus
without a C toolchain is possible later.)

### 4.5 RNS dependencies and the media boundary

LXST needs only `Link` (establish/identify/packet-callback/close), `Packet`
(`create_receipt=false`), `Destination` (IN/OUT SINGLE, announce, PROVE_NONE),
`Identity`, `Transport.{has_path,request_path}` — all present in
`leviculum-core`. The seam to host media is two functions:

```
encode_frame(codec_id, &[u8]) -> packet bytes
on_packet(&[u8]) -> Either<Signal, (codec_id, &[u8])>
```

Capture, playback, Opus/Codec2 DSP, mixing, filters, and soundcard I/O sit behind
host-supplied trait objects and never enter the `no_std` crate.

---

## 5. Audit of the existing Rust implementations

License note up front: **rsLXMF and rsLXST are both AGPL-3.0-or-later, the same
license as libreticulum.** Copying is therefore permitted; the earlier
"AGPL blocker" framing is wrong *for our project*. The vendored `opus-rs` is
BSD-3-Clause, also AGPL-compatible. The decision below is purely technical.

Both crates depend on a **different, unrelated** Rust Reticulum, `rsReticulum`
(crates `rns-crypto`, `rns-wire`, `rns-identity`, `rns-link`, `rns-transport`,
`rns-runtime`, `rns-interface`). We are **not** adopting that stack. Both are
std/tokio, neither declares `#![no_std]`.

### 5.1 rsLXMF — reference-only

- ~25.5k LOC, edition 2024. `lxmf-core` (~15.6k) + `lxmf-tools` (~6.4k).
- **std/tokio, baked in.** `tokio` is a direct dep of `lxmf-core`;
  `SystemTime::now()` is called inside `LxMessage::new` (`message.rs:137`);
  `std::fs::write/read` for paper I/O sit inside the message module
  (`message.rs:1003,1011`); `rand::thread_rng()` inside stamping
  (`stamper.rs:128`). This is the opposite of our injected-`Clock`/`Storage`
  model.
- **Pervasive `rsReticulum` coupling** in exactly the I/O-facing modules:
  `link_delivery.rs` (4475 LOC), `router.rs`, `propagation_*` all build on
  `rns_link::Link` / `rns_protocol::resource` / `rns_transport`. ~70% of the code
  is a full rewrite against our core, not a port.
- **A correctness defect we must not inherit:** the **message-level** stamp PoW
  uses a "simplified" iterated-SHA256 workblock (`stamper.rs:8-9`, used at
  `router.rs:486`, `message.rs:761,816`) instead of Python's HKDF-expanded
  workblock (`LXStamper.py:18-29`). Message stamps produced by rsLXMF will not
  validate against a Python peer — a Priority-1 compatibility bug. (The HKDF-
  correct construction exists only on its PN/peering paths.)
- **Tests:** 385 inline tests and strong `constants.rs` value-parity asserts, but
  **no Python-generated golden wire vectors** — which is how the stamp defect
  slipped through.
- **Usable as spec, not code:** `constants.rs` (field IDs, modes, costs,
  intervals — Python-parity-asserted) and the msgpack layout in
  `message.rs:251-633` (bin-vs-str, negative-fixint field keys, rmpv round-trip
  of complex fields) are the genuinely hard-won parts. Re-derive them
  independently against `reference/LXMF`; do not copy modules.

**Verdict: reference-only.** Mine `constants.rs` and the msgpack layout as a
checklist; treat Python as the authority; never copy `stamper.rs`'s message path.

### 5.2 rsLXST — adopt-partial

- 3 crates + vendored pure-Rust `opus-rs`. `lxst-core` (~3.2k LOC),
  `lxst-rns` (659), `lxst-telephony` (~3k + ~3.4k tests).
- **`lxst-core` has ZERO `rsReticulum` coupling** — by explicit design
  (`lxst-core/src/lib.rs:1-5`: "deliberately avoids audio and Reticulum runtime
  dependencies so packet/profile parity can be tested in isolation"). Its
  protocol modules touch `std` only in three trivial spots
  (`VecDeque`, `mem::take`, `f32::consts::TAU`), each with a `core`/`alloc`
  equivalent.
- **Clean, defensive, Python-tested protocol logic:**
  - `wire.rs` (485) — the `{0x00/0x01}` container, codec ids, profile base
    `0xFF`, `LxstPacket`/`Frame`/`Signal` (de)serialization. Crown jewel.
  - `profile.rs` (356) — all profile/signalling tables, all `const fn`.
  - `raw.rs` (204) — RawAudioFrame (de)serialization; honestly documents the
    float128 platform hazard rather than faking it.
  - `telephony.rs` (378) — a **pure, side-effect-free** signaling state machine:
    every transition returns `Vec<TelephonyAction>`, no I/O, no RNS, no time.
    Exactly the action-emitter pattern we want.
  - `stream.rs` (339) — `FramePacketizer` + `JitterBuffer`.
- **Real Python-parity discipline:** `python_wire_parity.rs` /
  `python_raw_parity.rs` shell out to actual `markqvist/LXST` and assert
  byte-for-byte both directions; `reference_snapshot.rs` pins the upstream commit
  + per-file SHA256 and fails on drift; `python_destination_parity.rs` checks the
  destination hash against RNS; `malformed_wire.rs` adds proptest fuzzing. This
  is precisely the reference-first discipline this project mandates, and the
  harness pattern is itself adoptable.
- **Reference-only parts:** `lxst-rns` (port its `pack_link_payload` header/encrypt
  recipe, `lib.rs:210-248`, against our Link — don't depend on `rns-*`);
  `lxst-telephony` async orchestration; `opus.rs` + `opus-rs` (std media, out of
  `no_std` scope, but a valuable correctness reference for a future std Opus,
  including its custom multi-subframe packing at `opus.rs:140-180`).
- **One real porting cost:** `wire.rs` uses `rmpv` (std today). The `no_std`
  port needs a `no_std` msgpack or a feature-gated alloc path.

**Verdict: adopt-partial.** Seed the new `lxst` crate from `wire.rs`,
`profile.rs`, `raw.rs`, `telephony.rs`, `stream.rs` with light edits
(`#![no_std]`, `core`/`alloc`, `rmpv` resolution, convert the two guarded
`.expect()`s in `stream.rs` per our no-unwrap rule), and adopt the Python-parity
harness pattern.

---

## 6. Proposed design

### 6.1 Crate layout

Two new `no_std + alloc` crates in the workspace, mirroring the
core/std separation already used by `leviculum-core` / `leviculum-std`:

```
lxmf/                 # #![no_std] + alloc, depends only on leviculum-core
  src/
    constants.rs      # field IDs, methods, costs, intervals (re-derived)
    message.rs        # pack/unpack, sign/verify, msgpack payload
    stamp.rs          # HKDF workblock PoW + ticket shortcut (feature "pow")
    ticket.rs         # ticket secrets, expiry windows
    delivery.rs       # method selection + per-method FSM (sans-IO)
    router.rs         # outbound/inbound queues, announce handling (sans-IO state)
    propagation.rs    # client side first; node serving later/feature-gated
lxst/                 # #![no_std] + alloc, depends only on leviculum-core
  src/
    wire.rs           # {0x00/0x01} container, codec ids  (seed: rsLXST)
    profile.rs        # profile/signalling tables          (seed: rsLXST)
    raw.rs            # Null + Raw codecs                   (seed: rsLXST)
    telephony.rs      # signaling state machine             (seed: rsLXST)
    jitter.rs         # jitter buffer policy                (seed: rsLXST stream.rs)
    media.rs          # host trait seam (encode/decode/capture/play)
```

Std-side conveniences (tokio driver glue, file persistence, host audio backends,
Opus/Codec2 FFI) live either in `leviculum-std`-adjacent helper crates
(`lxmf-std`, `lxst-std`) or behind feature flags — they are explicitly *not* in
the `no_std` crates.

### 6.2 Sans-IO contract (both crates)

Each crate is a state object the host ticks alongside `NodeCore`:

```rust
// Illustrative, not final.
impl LxmfRouter {
    fn handle_event(&mut self, ev: &NodeEvent, now_ms: u64) -> LxmfOutput;
    fn send(&mut self, msg: LxMessage, now_ms: u64) -> Result<LxmfOutput, LxmfError>;
    fn handle_timeout(&mut self, now_ms: u64) -> LxmfOutput;
    fn next_deadline(&self) -> Option<u64>;
}
// LxmfOutput carries the NodeCore calls to make (send_single_packet,
// connect, send_resource, ...) plus LXMF-level events (MessageReceived,
// DeliveryFailed, ...). The host wires LxmfOutput -> NodeCore -> transport.
```

This mirrors `NodeCore`'s own `TickOutput` model, so the two compose: the host
loop feeds bytes to `NodeCore`, passes resulting `NodeEvent`s to the LXMF/LXST
router, and dispatches both layers' actions. On hosts, a thin tokio adapter wraps
the loop; on nRF52 the existing embassy executor drives it.

### 6.3 Key decisions to settle before coding

1. **msgpack in `no_std`.** Pick `rmp`/`rmpv` with `alloc` (matches rsLXMF/rsLXST)
   vs a hand-rolled fixed-shape encoder. Must preserve exact type discipline
   (bin vs str, f64 timestamps, integer map keys, the announce-format first-byte
   sniff). Recommendation: `rmp` with alloc, validated by golden vectors.
2. **Ratchet crypto exposure (core change).** Either document `Destination` as the
   supported encrypt-to-destination path for PROPAGATED/PAPER, or expose a thin
   public wrapper over `Identity::encrypt_for_destination`. Decide before LXMF
   propagation/paper work.
3. **Compression (core change / feature).** LXMF must depend on
   `leviculum-core/compression` for Python wire-compat on large messages; confirm
   `libbz2-rs-sys` builds on the nRF52 target.
4. **PoW on constrained targets.** Gate stamps behind a `pow` feature; default
   cost 0 on MCUs; stream the workblock. Validate bit-exact against Python.
5. **Golden vectors.** Adopt rsLXST's reference-lock + Python-fixture harness for
   both crates from day one; capture byte-for-byte vectors from `reference/LXMF` and
   `reference/LXST` so we never repeat rsLXMF's stamp defect.

### 6.4 Compatibility guardrails (Priority 1)

- Title/content are msgpack `bin`. Timestamps are `f64`. Field keys are integers,
  including negative fixints.
- Stamp validity is a big-endian 256-bit compare; leading-zero-bit counting must
  match Python bit-for-bit.
- LXST signal channel is a variable-width int (`0xFF + profile` exceeds `u8`).
- LXST media is fire-and-forget Packets over a Link — no Resource/Channel.
- LXMF large messages use Resource with BZ2 compression — feature must be on.
- Every wire claim is backed by a golden vector captured from the Python
  reference, per this project's reference-first discipline.

---

## 7. Recommended staging

| Stage | Scope | Rationale |
|-------|-------|-----------|
| **0** | Land the two submodules (done); settle §6.3 decisions 1-3; stand up the Python golden-vector harness. | Removes the two core-touching unknowns and the interop safety net before any protocol code. |
| **1** | `lxmf`: message pack/unpack + sign/verify + OPPORTUNISTIC and DIRECT delivery over `leviculum-core`. Golden-vector tested. | The core 80% of real LXMF traffic; entirely `no_std`; no PoW, no propagation. |
| **2** | `lxmf`: stamps/tickets (feature-gated PoW), announce-driven stamp cost, inbound dedup. | Anti-spam parity with Python; isolates the RAM-heavy PoW behind a feature. |
| **3** | `lxst`: port `wire.rs`/`profile.rs`/`raw.rs`/`telephony.rs`/`jitter.rs` from rsLXST; Null/Raw codecs; signaling over `leviculum-core` Links; host media trait seam. | Reuses the clean, Python-tested rsLXST protocol layer; defers all media/DSP. |
| **4** | Std-side helpers: tokio drivers, file persistence, host audio backends, Opus/Codec2 (std-only); LXMF propagation **client**. | Host ergonomics and the parts that cannot be `no_std`. |
| **5** | LXMF propagation **node** serving (feature-gated, std-first). | Server-heavy, persistence-heavy; lowest embedded priority. |

---

## 8. Bottom line

- **LXMF:** fully feasible as `no_std + alloc` on `leviculum-core`. Build fresh,
  using `reference/LXMF` as the authority and rsLXMF's `constants.rs` + msgpack
  layout as a spec checklist only. Do not inherit its message-stamp PoW.
- **LXST:** feasible as `no_std + alloc` **for the protocol layer**; media stays
  host-side by design. **Adopt-partial** from rsLXST's `lxst-core` (wire,
  profile, raw, telephony, jitter) plus its Python-parity harness.
- **Both** ride the existing sans-IO `NodeCore` boundary, so they compose with
  the current embedded firmware path without a runtime. The only changes that
  reach into `leviculum-core` are small and known: ratchet-crypto exposure and
  the compression feature.
