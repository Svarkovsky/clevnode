# The USB control envelope

The LNode's transport CDC carries HDLC-framed Reticulum packets, plus a
small out-of-band control plane between an attached host (`lnflash`,
`lnsd`) and the firmware. Until Codeberg #238 that control plane was one
hand-cut magic per feature — a radio-config frame and a reset frame, each
recognised by shape. Three pending features each wanted a third magic,
which is how a channel becomes unextendable. This page documents the one
envelope every control frame rides in now, and how the two legacy magics
retire.

Wire truth lives in `leviculum-core/src/envelope.rs`; this page explains
it. If they disagree, the code and its tests win.

## Frame layout

One envelope per HDLC frame:

```text
[0xA4, 0xA5] [type: u8] [len: u16 BE] [payload: len bytes]
```

The length is strict: a frame whose payload is shorter or longer than
`len` is malformed. A reader that knows the envelope but not the type
answers a named refusal and stays in sync — the HDLC delimiter bounds the
frame, the header names what was skipped. Nothing envelope-shaped is ever
answered with silence; the legacy magics predate that rule and keep their
old manners (below).

## Frame types

Commands (host → board):

| type | name             | payload                                        |
|------|------------------|------------------------------------------------|
| 0x01 | RADIO_CONFIG     | the legacy frame's parameter block (13–19 B), no magic |
| 0x02 | RESET            | empty                                          |
| 0x03 | WALL_TIME        | unix seconds, u64 BE (8 B)                     |
| 0x04 | CAPABILITIES     | empty (a query)                                |
| 0x05 | TELEMETRY_TARGET | see below — set or clear the telemetry target  |
| 0x06 | TX_SPACING       | on-air transmit spacing in ms, u16 BE (2 B)    |
| 0x07 | RADIO_QUERY      | empty (a query, #349) — answered with RADIO_REPORT |
| 0x08 | FIXED_POSITION   | see below — set or clear the user-set position |
| 0x09 | MEDIA_PROFILE    | one flag byte (bit0 lora, bit1 ble) — answered with MEDIA_REPORT |
| 0x0A | MEDIA_QUERY      | empty (a query) — answered with MEDIA_REPORT |
| 0x0B | POSITION_SOURCE_QUERY | empty (a query) — answered with POSITION_SOURCE_REPORT |
| 0x0C | NODE_NAME        | see below — set or clear the operator-chosen name |
| 0x0D | NODE_NAME_QUERY  | empty (a query) — answered with NODE_NAME_REPORT |
| 0x0E | IDENTITY_QUERY   | empty (a query) — answered with IDENTITY_REPORT |
| 0x0F | ANNOUNCE         | empty — announce the LXMF delivery destination now (#376) |
| 0x10 | BLE_TX_GAP       | BLE inter-packet gap in ms, u16 BE (2 B), 0..=5000 (#376) |

Responses (board → host):

| type | name              | payload                                   |
|------|-------------------|-------------------------------------------|
| 0x81 | ACK               | `[acked_type]`                            |
| 0x82 | REFUSAL           | `[refused_type, reason]`                  |
| 0x83 | CAPABILITY_REPORT | `[version, accepted types...]`            |
| 0x84 | RADIO_REPORT      | the RADIO_CONFIG parameter block the radio is running (#349) |
| 0x85 | MEDIA_REPORT      | `[running_flags, configured_flags]` in the MEDIA_PROFILE flag encoding |
| 0x86 | POSITION_SOURCE_REPORT | one flag byte (bit0 fixed position set, bit1 GNSS built in and active) |
| 0x87 | NODE_NAME_REPORT  | `[flags, mesh_len, mesh…, ble_len, ble…]` — see below |
| 0x88 | IDENTITY_REPORT   | `[flags, identity(16), probe(16), lxmf(16)]`, 49 B fixed |

Refusal reasons: `0x01` unknown type, `0x02` malformed, `0x03` value
refused, `0x04` busy, `0x05` unsupported (the envelope layer knows the
type but this binary carries no consumer for it — retrying or rebooting
cannot help, only different firmware can), `0x06` not persisted (see
below), `0x07` no calendar clock (the command needs one and the board has
none yet — seed it with a GNSS fix or `--set-time` and retry). The
version in the capability report (`1`) names the envelope framing itself;
new frame types extend the accepted list without bumping it.

### ANNOUNCE (0x0F) and BLE_TX_GAP (0x10) — the #376 bench instruments

`ANNOUNCE` makes the board announce its LXMF delivery destination
immediately, on all interfaces, exactly as the telemetry path does before
a report — same destination, same app data, and the same clock gate:
without a calendar clock the board withholds the announce, logs
`[ANNOUNCE] withheld reason=no-clock` on the debug port and refuses with
reason `0x07`. (The gate is not cosmetic: the emission timestamp inside
the announce is what peers rank paths by — see
`docs/src/protocol-notes/announce-dedup-and-path-replacement.md` — so an
uptime-stamped announce would poison the path under measurement.) On
success the board logs `[ANNOUNCE] sent dst=<hex8> reason=host` and the
usual `BLE_TX_PKT` lines, and acks. One-shot; nothing is persisted.
Host side: `lnflash --announce`.

`BLE_TX_GAP` sets the gap the BLE drain leaves between the last fragment
of one packet and the first fragment of the next packet **on the same
connection handle**. With no value set the pumps serve the compiled
default of 100 ms (#376, the measured desk value —
`leviculum-ble-tx`'s `DEFAULT_TX_GAP_MS`); any set value overrides it,
`0` disables the gap entirely, and values above 5000 ms are refused
with reason `0x03`. Interface-layer only, per connection — the fan-out
and the core never learn of it — and volatile like TX_SPACING: a reset
restores the default. The board logs `[BLE ] tx_gap_ms=<n>` when the
value takes effect and `BLE_TX_GAP conn=<h> waited_ms=<n>` once per
deferred packet. Host side: `lnflash --set-ble-tx-gap <ms>`.

### NODE_NAME (0x0C) and NODE_NAME_REPORT (0x87)

The name an operator chooses for a board, replacing **both** derived
defaults at once — the LXMF announce's display name (`LNode-<hex8>`, what
Columba lists) and the BLE device name (`LN-<hex8>`, what a phone shows in
its Bluetooth settings). A board answering to two different names in two
places would be worse than the hex it replaced. The name is display only:
it never touches the identity, so two boards may carry the same name and
stay distinguishable everywhere it matters.

Set payload, the FIXED_POSITION set/clear shape on a variable-length
value:

```text
[set: u8] ([name: 1..=32 bytes of UTF-8])
```

`set` is `0x00` (clear, back to the derived defaults; 1-byte payload) or
`0x01`. No length byte — the envelope header already carries the frame
length. The 32-byte bound is **airtime policy, not a wire limit**: the
name rides in every announce, so `leviculum_core::node_name` derives it
from the announce's on-air cost and `leviculum-lxmf/tests/
announce_name_airtime.rs` pins every number in that derivation. Invalid
UTF-8, control characters, surrounding whitespace and an over-long name
are all refused as malformed rather than silently shortened: a name that
arrives different from the one that was typed is worse than an error.

The report answers both frames:

```text
[flags: u8] [mesh_len: u8] [mesh…] [ble_len: u8] [ble…]
```

`flags` bit0 is "a name is stored" (as opposed to both names being
derived) and bit1 is "the BLE surfaces are one reset behind". Unknown bits
are kept, not refused.

The two names are the **effective** ones, not the stored record, because a
host cannot derive either: the two defaults are different strings built
from an identity hash the host never sees, and the BLE name is
additionally shortened to `leviculum_ble_tx::DEVICE_NAME_LEN` (11 bytes)
on a codepoint boundary. They also adopt the name at different moments —
the mesh name is in force for the next announce, while the advertisement
was built once at boot and cannot be rebuilt under a live SoftDevice — and
bit1 is the board saying so. That is the MEDIA_REPORT
running-versus-configured argument on a second feature.

A board that has not yet published its identity hash (USB comes up several
statements into the firmware's `main`, the node only after the LoRa
bring-up's awaited SPI transactions) answers `busy` and applies nothing,
so the host's retry is a real retry. `unsupported` is reserved for a
binary that carries no name gate at all.

### What an answer on the persist path means (#358)

Four frames write a flash record: TELEMETRY_TARGET (0x05),
FIXED_POSITION (0x08), MEDIA_PROFILE (0x09) and NODE_NAME (0x0C). For
those four the answer carries a durability promise:

> **When the client's call returns, a reset cannot lose the setting.**

The board therefore does not answer them until its store task confirms
the record is on the page. An ACK — or, for the media profile and the
node name, their report — means written, not merely applied. A write the store task
gave up on comes back as a refusal with reason `0x06`: the board *is*
running the value, and cannot promise it survives a reboot. That is a
different sentence from `busy` (retry) and from `value refused` (the
value was fine), so a client can tell it apart and say so.

The wait is bounded at 2.5 s, inside the 3.5 s window `lnflash` gives one
control conversation; a store task that never confirms is reported as
`0x06` rather than left holding the port. Until #358 the answer went out
between the RAM apply and the page write, so a scripted `set` followed by
a reset — periculum's per-scenario media application, `lnflash`, any
automation — could reboot the board inside the window and lose the
setting. A sleep in front of the reset does not close it: the store task
may be working an earlier queued write, and a constant cannot bound a
queue.

### The media-profile frames

```text
[flags: u8]   bit0 = lora, bit1 = ble; set means the carrier is enabled
```

Both media frames are answered with a MEDIA_REPORT rather than an ACK,
because the two profiles it carries can honestly differ. `running` is
what the board is carrying traffic on right now; `configured` is what a
reset would come up with. They part exactly when a carrier that did not
come up at boot is switched on: the board has no driver task to start,
and an ack would claim it did. A flag byte with a bit outside the two
known carriers is `malformed`, never masked down to "that carrier is
off" — the firmware does not get to invent a reading of a carrier it
does not know.

The default, for a board with no stored profile, is both carriers on:
absence of a record must change nothing about a fielded board. Concept
and semantics: `docs/src/concepts/media-profiles.md`.

The wall-time frame calls the calendar seam
(`set_wall_time_unix_secs(.., TimeSource::Host)`); the seam's sanity
window decides between the ack and a `value refused` refusal, and an
accepted seed logs `[TIME_SEED] source=host` and flips the banner's
`[TIME_SOURCE]` to `host` — the exact mirror of the GNSS path.

### The transmit-spacing frame (#345)

```text
[spacing_ms: u16 BE]
```

The gap the board's LoRa interface leaves between the end of one packet's
airtime and the key-up of the next. It is applied inside
`transmit_all_frames`, the last thing before the radio is keyed, so it is a
gap between two packets on the air rather than between two hand-overs, and
whatever the transmit path already spent since the previous packet ended
(the CAD, the SPI traffic, the log lines) is counted against the requested
gap rather than added to it. The split frames of one packet are unaffected:
they still go out back-to-back, because the receiver's reassembler requires
that.

Every u16 value is legal, `0` included — `0` is the compiled default and
imposes nothing, so the only malformed frame is one of the wrong length.
The value is **not persisted**: it is a measurement instrument (the sweep of
the telemetry announce/report spacing, #345), and a reset returns the board
to the default. The board logs `[LORA_TX_SPACING] intended_ms=… waited_ms=…
gap_ms=…` at every key-up; `gap_ms` is the gap that was measured, and `-1`
is the first packet since boot, which has no previous airtime edge to be
measured from.

`lnflash --set-tx-spacing <MS>` is the host side.

### The telemetry-target frame (#236)

```text
[profile: u8] [dest_hash: 16] [key_present: u8] ([public_key: 64])
```

`key_present` is `0x00` or `0x01`, never inferred from the length: per
the #236 UX decisions (2026-08-22) the public key is optional and
hash-only is the common case — the user knows the LXMF address, the node
resolves the key over the air.

Profile ids:

| id   | name    | meaning                                              |
|------|---------|------------------------------------------------------|
| 0x00 | OFF     | **clear the target** — telemetry off                 |
| 0x01 | TRACKER | movement-driven cadence                              |
| 0x02 | STATION | slow heartbeat only; the default profile             |

`0x00` is the clear encoding. It rides in the profile slot rather than
in a magic destination hash because that slot's whole job is to say
which cadence applies, and "none" belongs in its vocabulary; the rest of
the payload is still parsed and must still be well formed, so a clear
frame is not a licence to send a short one. The destination hash and key
of a clear frame are ignored, and `encode_telemetry_clear` zeroes them
rather than echoing a target back for no reason.

An id the firmware does not know is **not** a refusal: the destination
is kept and the default profile's cadence runs, because a newer host's
cadence preference is not worth losing a configured target over. Which
profile is actually running is in the board's `[TELEMETRY]` banner.

Firmware from before #236 answers this type with an `unknown type`
refusal and leaves it out of its capability report, which is precisely
how a #236-aware host detects a pre-#236 board.

#### How `lnflash` drives it

Telemetry is configuration, not firmware, so the same frame is reachable
from the flash flow and without flashing anything:

| flag                                | effect                                                        |
|-------------------------------------|---------------------------------------------------------------|
| *(none)*                            | after the radio step: `Send telemetry? [y/N]`, default **no**  |
| `--telemetry <ADDRESS>`             | implies yes; 32 hex chars, spaces/colons/case tolerated        |
| `--telemetry-profile <tracker\|station>` | which cadence; default `station`                         |
| `--telemetry-key <128 hex>`         | the key-present form; absent = hash-only, the common case      |
| `--no-telemetry`                    | send profile `0x00` — clear whatever the board had stored      |
| `--set-telemetry`                   | the same configuration on running boards, no flash             |

Answering *no* at the prompt sends **nothing**; `--no-telemetry` sends a
clear frame. The difference matters on a board that already has a target:
silence leaves it, the clear frame removes it.

A yes needs exactly one input — the LXMF address — because that is what
users have. Nothing detects a terminal: `Ui::ask` answers "no answer" for
`--yes` and for a piped or closed stdin alike, and every prompt treats
that as its stated default, so a scripted run cannot block.

What the host reports back is the ack. The node's own
`[TELEMETRY] target=… state=off|no-position-source|awaiting-key|ready`
line goes to the debug CDC (if00), which `lnflash` holds open only for
the post-flash boot check — so it is named as the place to read the rest
rather than read back over a second connection.

**The consequence sentence.** A target alone does not make a board
report: sending the position is the switch for sending everything
(`docs/src/concepts/telemetry.md`), so a board with neither a fixed
position nor a GNSS receiver stores the target and stays silent. After
an ack, `--set-telemetry` therefore asks the board itself
(POSITION_SOURCE_QUERY, on the same open port) and, when the answer is
"neither", says so:

```text
3-2.4: target stored; nothing will be sent until a position source
       exists — set one with --set-position.
```

Honest, not a refusal: the target is valid configuration and it *is*
stored. A board that answers with a source is told nothing of the kind,
and a board that does not answer the query at all — firmware without it,
or a binary with no reporter, which refuses it by name — is told nothing
either. Guessing here would put a false warning in front of an operator
whose board is fine.

### The fixed-position frame

```text
[set: u8] ([latitude_e6: i32 BE] [longitude_e6: i32 BE]
           [alt_present: u8] ([altitude_e2: i32 BE]))
```

A user-set position as the telemetry source. `set` is `0x00` (clear, the
1-byte payload is the whole command) or `0x01`; `alt_present` follows the
telemetry target's key-present rule — an explicit flag byte, never
inferred from the length. Units are the telemetry wire's own scaled
integers: degrees × 1e6, metres × 1e2, so the coordinates the user typed
are the coordinates that go on the air. A latitude beyond ±90° or a
longitude beyond ±180° is refused as malformed.

Semantics (decided 2026-08-30): **while set, the fixed position replaces
the position sensor entirely, in every profile** — no blending, no
fallback surprises — and the explicit clear returns the node to sensor
reporting, which for a GNSS-less binary means no position. The board
persists it beside the telemetry target (same flash page, so it survives
resets and UF2 updates), marks the source in its report line as
`possrc=fixed|gnss`, and puts it on the wire in Sideband's own
fixed-location shape: accuracy 0.01 m, speed and bearing 0, altitude 0
when unset (`Location.update_data`, synthesized branch, Sideband
`2000d81`).

The ack is capability-gated exactly like the telemetry target's: only
the reporter reads the position, so a binary without one answers the
`unsupported` refusal rather than acking a pin nothing will ever report.

#### How `lnflash` drives it

| flag                          | effect                                                   |
|-------------------------------|----------------------------------------------------------|
| `--set-position LAT,LON[,ALT]`| set it on every running board, then exit; no flash       |
| `--clear-position`            | back to sensor reporting                                 |

The value is decimal degrees, comma or space separated, sign or
hemisphere letter (`52.52,13.405,34`, `"52.52N 13.405E"`, `36.85S,73.04W`
all parse; a letter and a sign together do not). The optional third value
is the altitude in metres. Degrees/minutes/seconds notation is refused by
name rather than misparsed.

## Why an envelope frame can never be a packet

The channel's other occupant is HDLC-framed Reticulum traffic, so every
control frame must be unmistakable. Three facts hold it:

1. The first magic byte `0xA4` has the IFAC bit set, and this channel
   runs without IFAC — no peer on it emits a packet whose first byte
   matches, and firmware from before the envelope drops a received
   envelope frame in packet parsing for the same reason.
2. Every frame a host may send *before* it knows the peer speaks the
   envelope — the capability probe, wall time, reset — is shorter than
   the 19-byte minimum Reticulum wire packet, so it cannot be
   packet-shaped at all.
3. Frames at that size or beyond (radio config at 24 B, telemetry target
   at up to 87 B, a set fixed position at exactly 19 B) are only sent
   after a capability report proved the peer is envelope-speaking
   firmware. This ordering is load-bearing: an envelope speaker must
   probe before it sends any envelope frame of 19 bytes or more.

## Compatibility window, and how it retires

The two legacy magics stay accepted, with their legacy answers, so both
field directions keep working:

- **Old host tool → new firmware:** the legacy 21-byte config magic and
  the 4-byte reset magic are classified ahead of the envelope
  (`classify_control_frame`) and answered with the legacy two-byte-style
  acks (`RADIO_CONFIG_ACK`, `RADIO_RESET_ACK`). An invalid legacy config
  keeps its historical silence; audible refusals begin with the envelope.
- **New host tool → old firmware:** `lnflash` opens every control
  conversation with a capability probe. Firmware that answers gets
  envelope frames; firmware that stays silent (pre-envelope) gets the
  legacy config magic as a fallback, and `--set-time` reports "this
  firmware predates the control envelope" by name instead of guessing.

`lnsd` still speaks the legacy config magic on every connect; it migrates
to the envelope in its own batch.

Retirement happens in that order: first `lnsd` and every shipped host
tool speak the envelope (probing, with fallback), then — after a release
cycle in which lnflash bundles only envelope-speaking firmware, so any
field board a current tool meets accepts it — the firmware drops the two
legacy classifier arms and the host tools drop the fallback. Each step is
observable: a host that still needs the fallback logs it, and a board
that still receives legacy magics is running firmware older than the
bundle that introduced the envelope.

## Adding a fourth frame type

The definition of done for #238: allocate the next type constant in
`leviculum-core/src/envelope.rs`, give it a payload codec with tests, add
a `ControlAction` variant and its executor arm in
`leviculum-nrf/src/usb.rs`, and append the type to
`ACCEPTED_CONTROL_TYPES` so the capability report advertises it. The
framing, the refusal path, the probe, and both host speakers stay
untouched.
