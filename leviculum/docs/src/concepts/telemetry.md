# Telemetry

How a node reports where it is and how it is doing, and why almost none
of that format is ours to decide. This applies to every firmware and
every platform port, present and future.

## The goal

A node — a tracker in a rucksack, a solar relay on a roof, a handheld
with a display — produces readings that belong somewhere else: a
position, a battery charge, a link quality. Telemetry is the path from
that reading to a map pin or a database row, over the same mesh the node
already speaks.

The receiving end is not ours. Sideband and Columba display telemetry
today, from any peer, without knowing what produced it. A node that
emits the format they already read is useful on the day it ships; a node
that emits its own format is useful when someone writes a viewer for it.
So the format is adopted first and the node conforms to it, not the
reverse.

## Where the format comes from, and how far it is settled

Telemetry rides inside a normal LXMF message, in the `fields` dictionary
of the encrypted payload — not in a destination of its own, not in a
side protocol. It therefore inherits everything LXMF already provides:
end-to-end encryption, the three delivery methods, propagation nodes,
and receipts. A peer that does not know the field sees a message with no
text, which is the correct failure — subject to the empty-content rule
below.

The field numbers are LXMF's (`leviculum-lxmf/src/constants.rs:43-69`).
Three matter here: `FIELD_TELEMETRY` (0x02) carries one node's readings,
`FIELD_TELEMETRY_STREAM` (0x03) carries many nodes' readings collected
by a third party, and `FIELD_COMMANDS` (0x09) carries the request that
asks for the second one.

The *content* of `FIELD_TELEMETRY` is Sideband's, defined by its
`Telemeter` class in `sense.py`: a msgpack map from sensor ID to that
sensor's packed value. **Sideband is the origin of this format and, for
most of it, the only implementation.** Columba reimplements two sensor
IDs of the twenty-four — time and location — in `TelemeterCodec.kt`,
naming `sense.py` as its own reference. That is corroboration for those
two, not a second independent implementation of the format. For every
other sensor we encode, we are the second implementation, and the
accept-set rule of [Python-RNS
compatibility](python-rns-compatibility.md) applies with full force: we
must read what the origin *emits*, not only what we would have written.

How settled the format is differs by part, and the difference decides
how much freedom a reader has:

| Part | Status |
|------|--------|
| `FIELD_TELEMETRY`, time and location sensors | Two implementations agree. Settled. |
| Other sensor IDs | One implementation. Follow `sense.py` exactly. |
| The collector exchange (`FIELD_COMMANDS` / `FIELD_TELEMETRY_STREAM`) | Two implementations that **disagree today**. See the wire table below. |

> **Why citations to Sideband and Columba carry no line numbers.** This
> is a new rule, stated here rather than inherited: those repositories
> are not in this tree and not pinned by it, so a `path:line` citation
> into them cannot be checked by the citation guard and cannot be
> trusted to still point at what it claims. They are cited by symbol,
> which survives the edit that moves a line, plus the upstream commit
> the reading was taken from — Sideband `2000d81`, Columba `0930293` —
> so a reader can reconstruct the line. [Checks that are actually
> checks](checks-and-citations.md) argues the converse case for
> in-tree references and does not cover out-of-tree ones; this is the
> gap being filled, not an application of that page.

## Which viewer wins a disagreement

Columba is the app this project targets: it is the one whose users we
serve first and the one we can realistically patch. That ordering
decides what we *build for*. It does not decide what is correct on the
wire, and the two questions are answered in this order:

1. **Does one form break the other implementation?** Then that form is a
   bug, not a tie, however confidently it is implemented. Emit the form
   both accept and report the bug to whoever emits the other.
2. **Are both forms legitimate readings of the format?** Then prefer the
   one Columba displays.
3. **Would our output only be understood by a build carrying our own
   patch?** Then it is not permitted, whatever step 2 said.

Step 3 is the operative test, and it is the one that can be checked
before the change lands rather than after: run the output against an
unpatched build of both viewers. Three concrete temptations it rules
out, each of which would work and each of which would leave our node
broken against every implementation but the patched one — a receiver
taught to accept an unverifiable signature so a node can skip announcing
its delivery destination; a receiver taught to ignore timestamps
entirely so a node can skip keeping a calendar at all; a receiver
taught to reinterpret a placeholder coordinate so a node can transmit
without a fix.

## Rules that hold regardless of platform

### Telemetry always stamps — never ahead of the calendar estimate

`SID_TIME` and the location sensor's `last_update` are real UTC seconds,
and they are load-bearing at the receiver in three separate places:

- **Storage keys on them.** Sideband dedups on exact `(source, ts)`
  equality and otherwise inserts, so a wrong timestamp is stored rather
  than rejected — a node emitting a wrong time appears in the viewer,
  wrongly dated.
- **Collection filters on them.** A collector serves only rows strictly
  newer than the requester's stored cursor.
- **The cursor advances over every row seen, saved or not.** So one row
  timestamped in the future raises a requester's cursor past it
  permanently: every honest later reading from that collector is "not
  newer" forever, and nothing anywhere logs a refusal.

That third mechanism is why stamping is directional. A future stamp is
poison — permanent, silent, and it hurts every subscriber of a
collector, not the sender. A past stamp merely sorts too far back:
visible, attributable, harmless. An earlier version of this page drew
the conclusion that a node without a verified clock must not report at
all ("arms 1–3 or silence"); that rule is withdrawn. It silenced
exactly the switch-on-and-it-works trackers this feature exists for,
and it defended the cursor at the wrong end.

The binding rule comes from the anchor model of [Time and
clocks](time-and-clocks.md): **a telemetry node always stamps with
its best honest calendar estimate, and never ahead of it** — every
fallback anchor lies in the past, so an uncertain calendar errs
backwards by construction; a source that is wrong but plausible is
stamped as-is, the residual the anchor model names. A node whose
calendar is still at the build floor reports recognisably old
readings, and the cost is priced per path. Where the reading is
displayed directly, it sorts to the back of the viewer: visible,
attributable. Where it travels through a collector, the real price
is higher: a collector serves only rows above a requester's cursor,
so a birth-stamped row sits below every already-synced requester's
cursor — not sorted backwards but **invisible to that requester,
indistinguishable from loss**, until the tracker heals. The cost
ends at the node's first plausible contact — outright at
arms-1–3-quality time, provisionally when healed from traffic
([the one honest cost](time-and-clocks.md#the-one-honest-cost)).

The cursor is defended at the reader instead: our collector (#239)
clamps inbound stamps beyond its local plausible-now to receive time
for indexing and cursor advancement, keeping the original stamp as
local display information ([ingress
clamping](time-and-clocks.md#ingress-clamp-for-semantics-keep-for-display)).
The clamp is armed only while the collector's own calendar is healed:
an unhealed collector clamping "to receive time" would drag every
honest current stamp down to its own ancient notion of now and
blackhole them below every synced cursor. Until it heals, it takes
in-window sender stamps as-is — the same stamps double as its
healing evidence — and rows ingested before healing keep their index
stamps; there is no re-index. Dedup keys are never clamped: dedup
runs on content and transient ID, the clamp covers ordering and
cursor semantics only. And what a collector serves is the clamped
value — a stream row has one timestamp slot — while keep-for-display
stays local; note the stated limit that the row's `packed_telemetry`
still carries the sender's raw `SID_TIME` claim to any downstream
parser. Foreign future stamps then cannot starve anyone's cursor
through us.

A node still has to know *which* arm anchored its calendar — the
provenance requirement of [Time and
clocks](time-and-clocks.md#record-the-source) — not as a gate on
reporting but as the diagnosis surface: "why does this tracker report
from 2026-01-01" must be answerable from the node itself.

### No fix, no position — and absence has one encoding

A sensor with no reading is absent from the map. It is never present
with a placeholder: no zero coordinates, no last-known value restamped
as current, no accuracy invented to fill the field. A viewer cannot
distinguish a placeholder from a measurement, and 0°/0° is a real place
in the Gulf of Guinea.

The format does not decide how absence is spelled, so we do. Sideband
packs every *active* sensor, and a sensor with no data packs as its SID
mapped to `None` — key present, value empty. That is two encodings of
absence in one format, and the choice is ours to fix:

- **We emit the omission.** A sensor without a reading contributes no
  key. `Telemeter.from_packed` instantiates only the sensors present in
  the map, so an omitted key round-trips cleanly.
- **We accept both on read.** A SID mapped to `None` is a sensor without
  a reading, not a malformed message. This is the accept-set half of
  [Python-RNS compatibility](python-rns-compatibility.md#level-1-wire-and-semantic-compatibility):
  the origin emits a form we would not write, and refusing it would be
  our defect.

### The encoder is multi-sensor from the first line

The unit is a Telemeter with *n* sensors, not a position packet with
extras bolted on later. Position, battery, temperature and link quality
are the same code path with different sensor IDs, and every board that
measures anything gets to report it without a second design.

The justification is the format's own extension mechanism, not our
future plans: a viewer ignores a sensor ID it does not know, so emitting
a reading no current viewer displays costs one map entry and breaks
nothing. A sensor left out of the encoder, by contrast, needs a new
design to add later.

Followed through on the boards: **a node reports everything it can
measure**, so both LNode builds carry the nRF52's die temperature as a
Temperature reading (SID `0x07`) whether or not a baseboard is fitted.
The die thermometer belongs to the SoftDevice, so it is read through
`sd_temp_get` and never off the TEMP registers; its 0.25 °C step is
already exact at the two decimals Sideband rounds to, so the value goes
on the wire unrounded. Battery charge stays feature-gated — it needs a
baseboard that has a gauge — and an absent sensor contributes no key.

### A reporting message carries no text

A telemetry message sets `content` and `title` to empty. This is a hard
wire requirement, not tidiness: Sideband suppresses the notification for
a telemetry-bearing message only when both are empty, so a reporting
node that fills in either one notifies its recipient once per reporting
interval, forever, and the feature is indistinguishable from spam.

### Delivery is opportunistic unless a port shows otherwise

Of LXMF's three methods, a reporting node uses the opportunistic single
packet by default. A direct delivery pays a link setup — three
round trips before any payload — for a payload of roughly fifty bytes,
and a propagated delivery pays a node round trip and adds a delay that
makes a position stale. A port that needs delivery confirmation, or that
reports to a target it can only reach through a propagation node, may
choose differently, and states why.

### Cadence is policy; airtime is the interface's business

A telemetry producer states *when it wants to report*: a minimum
interval, a minimum distance moved, a maximum interval as a heartbeat so
that "stationary" stays distinguishable from "dead", an accuracy
threshold, and a settle time so a cold start does not spend the channel
on a drifting first fix.

It does not state *when the radio may transmit*, and it holds no airtime
figure of its own. Duty cycle, spacing and back-off belong to the
interface — see [Interface isolation](interface-isolation.md) and
[Regulatory airtime](regulatory-airtime.md), which also settles how any
such figure is to be described: modelled, and a floor rather than a
total, because the board's own meter clears when it is read.

The configuration surface itself is settled in #236: setting a target
*is* the on-switch, and the tracker and station profiles bundle the
cadence defaults.

### Activation is configuration, not firmware

Setting or changing the telemetry configuration never requires
rewriting firmware. `lnflash` gains a config-only session — the same
post-flash serial configuration channel, entered without a UF2 write —
and the #238 control envelope and #235 remote management make the same
configuration changeable at runtime later. The reason is operational: a
node already running in the field must be adoptable into telemetry, and
retirable from it, where it hangs.

**One input, and it is the address.** Most users configure nothing
beyond the target, so the target is the only thing configuration may
*require*: profiles bundle the cadence and one of them is the default
(station — a node that does not move is the common case, and a tracker
misconfigured as a station still proves it is alive on the heartbeat,
where the converse merely costs airtime). Everything else is an expert
flag underneath a preset, in the same shape as the radio menu.

### Sending the position is the switch for sending everything

Reports go out iff **both** a target is configured **and a position
source is** — a fixed position set, or a GNSS receiver built into the
firmware and not switched off (Lew, 2026-08-30). The reading is
**intent, not possession**: a receiver that has never seen sky still
counts, because the four use cases behind this feature — tracker,
sensor node, mobile transport node, quasi-modem — are all nodes whose
operator meant to say where they are, and a tracker that goes silent in
a garage is indistinguishable from a dead one. Such a node keeps
reporting on the heartbeat with the position absent and its other
sensors fresh, which is the designed behaviour and not a degraded mode.
GNSS is optional throughout; a fixed position is settable on any board,
with or without a receiver.

What the rule refuses is the *unconfigured* node: a target and no
answer of any kind to "where am I". It sends nothing — not a heartbeat,
not a battery reading — and says why, `state=no-position-source` on the
same surface and the same cadence rule as `awaiting-key`. Position-less
telemetry is deliberately **not** a mode: a node that reports readings
a collector cannot place is a row on a map with nowhere to put it, and
the operator who wanted one had only to set a position. Setting one, or
flashing a build that has a receiver, leaves the state at once — it is
a runtime path, like applying a target, and needs no reboot.

### The destination hash alone is enough

A user knows the LXMF address. Requiring the public key alongside it
would make the common case the hard one, so **the key is optional**
wherever a target is set: hash-only is expected, and the node resolves
the key itself over the air — a path request is answered with the
destination's announce, and the announce carries the identity.

This creates a state that did not exist when a target implied a key: the
node holds a perfectly valid target it cannot yet encrypt to. That state
is **stated, not waited out silently**. A reporting node says which of
four it is in — `off`, `no-position-source`, `awaiting-key`, `ready` —
on the same surface as its clock provenance, and a target that never
resolves is then a visible condition rather than an absence of packets.
`no-position-source` outranks `awaiting-key` in that line: a node that
will not report anyway must not spend airtime chasing a key it has no
use for.

The consequence for the previous rule is that the immediate report
fires **on key arrival**, which for a hash-only target is later than the
moment the target was set. A frame that carries a key skips that wait,
which is the whole benefit of carrying one.

### Setting a target emits one immediate report

When a telemetry target becomes usable — set or changed with a key, or
set by hash and then resolved — the node sends one report at once,
regardless of the configured cadence. Success must be observable within
seconds: a station profile on an hourly heartbeat would otherwise leave
the operator without any confirmation for up to an hour. The immediate
report follows every other rule in this document — no fix, no position;
empty content and title.

The report is owed until it is actually sent, not until it was first
due: a node that has the key but no path yet retries rather than
counting an attempt it could not make. And a report that a target
cannot receive is a report nobody can verify, so the node announces its
own delivery destination before sending — the announce is what puts our
public key in the receiver's hands.

### A request from the target triggers one report

Sideband lets a peer ask for telemetry on demand: an LXMF message whose
`FIELD_COMMANDS` field (`reference/LXMF/LXMF/LXMF.py:16`) carries the
`TELEMETRY_REQUEST` command with a timebase. A reporting node answers
one such request with an immediate report — the same report a target
write arms, subject to the same rules (owed until sent, announce first,
attempt floor). That gives the operator a position on demand instead of
at the profile's cadence, and gives a field test a way to force the
whole chain — path request, answer, data — at a known moment.

The gate is authentication, not reachability: the request must name the
configured target as its source *and* carry a signature that verifies
against the target's identity. **The target is the only allowed sender
for now**; a general allow list is a later step and will follow the
collector rule above — explicit, empty by default, set only off-radio.
A request from anyone else is answered with nothing but a debug line,
because an unauthenticated trigger for a radio transmission is a remote
airtime primitive.

Requests are rate-limited to one accepted request per `min_interval_ms`
of the active profile; a request inside the window is logged and
dropped, not queued. The request's timebase is read but does not change
the answer: a node keeps no history, so the current reading is the
answer to every timebase.

Sideband's own source is not vendored under `reference/`, so the
command id (0x01) and the value shape (a list of command maps,
`[{0x01: <timebase>}]`) are a **documented assumption** — recorded at
`COMMAND_TELEMETRY_REQUEST` (`leviculum-lxmf/src/telemetry.rs:678`) —
to be verified against a captured Columba/Sideband request and then
pinned as a fixture.

On the host side, `lnsd` neither registers an `lxmf.delivery`
destination nor runs a telemetry policy today, so there is nothing for
a request to trigger there; the screen in `leviculum-lxmf` is the seam
a host-side consumer will reuse when that changes.

### Fan-out is the expensive shape; collection is the cheaper one

Telemetry to *n* recipients is *n* individually addressed and
individually encrypted messages. Mutual reporting in a group of *n* is
therefore *n*(*n*−1) messages per interval — thirty a minute for six
people at a one-minute cadence.

Routing the same group through a collector is *n* reports plus, for
those who want to see the others, *n* request/response pairs: eighteen
rather than thirty for the same six. The saving is real but it is
roughly a third, not fivefold, and it is not free in kind — a stream
response carrying several sources will exceed one packet and become a
link plus a resource transfer, where a report is a single packet. Both
figures belong in a port's own arithmetic; neither is a licence to skip
it.

There is no third shape. Reticulum has no multi-hop broadcast at all —
see [Public channels over
LXMF](public-channels.md#1-why-the-obvious-approach-does-not-work), which
settles this at the protocol level rather than by what two apps happen to
implement. LXMF's `FIELD_GROUP` is conversation metadata and not a
delivery mechanism; it would not produce a broadcast even if every
viewer read it.

### Relaying someone else's readings needs permission

A collector redistributes positions of people who are not asking for
that redistribution. It therefore answers requests only from an explicit
allow-list, empty by default, set only through a path that is not the
radio: on a host that is the config file, as `remote_management_allowed`
is (`leviculum-std/src/config.rs:95`, empty by default at
`leviculum-std/src/config.rs:213`); on a firmware with no filesystem it
is the local control channel, which is the same requirement in a
different envelope.

The same applies to precision. Where a deployment wants a node's
position blurred, the blurring happens at the producer, before the
reading is packed — a receiver's promise to round a coordinate is not
privacy.

### A port may ship send-only

Emitting telemetry needs an encoder, a target and a cadence. Consuming
it needs the inbound LXMF path, a peer table and something to show. A
port may implement the first without the second, and nothing in this
document may be read as requiring both: a tracker that cannot receive is
a complete node for its purpose.

## The collector exchange

Two implementations exist and they disagree, so this table is normative
for us rather than descriptive of them.

| Element | Form | Note |
|---------|------|------|
| Request | `FIELD_COMMANDS` = list of single-entry maps | Sideband's `Commands.TELEMETRY_REQUEST` is key `0x01` |
| Request argument | `[epoch_seconds, is_collector_request]` | An **absolute UTC epoch**, not an interval or an age |
| Legacy request | `{0x01: epoch_seconds}` — bare scalar | Must be **accepted** on read; the origin still emits it and infers `is_collector_request = true` |
| Response | `FIELD_TELEMETRY_STREAM` = list of rows | |
| Row | `[source_hash, timestamp_seconds, packed_telemetry, appearance]` | **Always four elements**, `None` in the fourth when there is no appearance |

The four-element rule is the one that costs something. Sideband indexes
the fourth element unconditionally, so a three-element row raises inside
its ingest and aborts processing of the **whole message**, not just that
row — while Columba's native collector emits three elements when
appearance is absent, and its own Python-backed collector emits four.
Under the ordering above this is step 1, not step 2: one form breaks a
conforming receiver, so we emit four and the three-element form is a bug
to report.

A collector additionally never serves a row whose timestamp lies in
the future relative to its own healed clock. Serving one permanently
starves the cursor of every requester that sees it. Under [ingress
clamping](time-and-clocks.md#ingress-clamp-for-semantics-keep-for-display)
such a row cannot enter the index while the clamp is armed — the
stamp is clamped to receive time on ingest — so for a healed
collector this serving rule is defence in depth. It is the primary
barrier for exactly one population: rows ingested *before* the
collector's own calendar healed, which were taken as-is and keep
their index stamps (there is no re-index).

## What a reporting node owes

- **An announced delivery destination.** A receiver verifies the LXMF
  signature against the sender's public key, which it can only have from
  an announce. A node that reports announces its delivery destination,
  with a display name in the announce data — otherwise the reading is
  unverifiable and the pin, if it appears at all, is a hex string.
- **The target's public key, before anything else.** Encrypting to a
  destination is impossible without it. There is no broadcast around
  this: the reference's transmit-on-all-interfaces branch
  (`reference/Reticulum/RNS/Transport.py:1177-1182`, our equivalent
  `send_on_all_interfaces`, `leviculum-core/src/transport.rs:3006`)
  applies to a packet that already exists, and building one required the
  key. So a port either preconfigures the target identity or waits until
  it has heard the target announce.
- **A path, or a request for one.** `send_to_destination`
  (`leviculum-core/src/transport.rs:3111`) fails without a path entry.
  The primitive for obtaining one is `request_path`
  (`leviculum-core/src/node/mod.rs:2956`); a node with the key but no
  path asks and waits rather than giving up.
- **An out-of-band trust step at the receiver, in the operator's hands.**
  Sideband can be configured to ingest telemetry only from trusted
  peers, and both viewers gate *collector requests* on an explicit
  allow-list. When that step has not been taken, a correctly reporting
  node is silently invisible, and it looks exactly like packet loss.
  Documentation of a reporting port says so; a diagnostic that cannot
  distinguish the two cases is worth building before the third support
  question arrives.

## The extension ladder

The format is fixed by implementations we do not control, so extending
it is a cost with a blast radius, not a design choice. Every addition
climbs from the bottom and stops at the first rung that works.

1. **An existing sensor ID already carries it.** Sideband defines
   twenty-four, well beyond position: battery, temperature, pressure,
   physical link, power production and consumption, and free-text
   information. A reading that fits one of them is not an extension, and
   emitting it costs nothing even where no viewer shows it yet.
2. **`FIELD_CUSTOM_META` (0xFD) carries it — under someone else's
   keys.** This slot is *not* free. LXMF reserves it for private use,
   and Columba has claimed it: it carries an unnamespaced map with the
   keys `cease`, `expires`, `approxRadius` and `ts`, and a truthy
   `cease` makes Columba **delete the sender's whole track**. There is
   one value per message. So a node targeting Columba may emit
   *Columba's* semantics here — a bounded sharing session that expires
   itself is the case that earns it — and may not put its own
   vocabulary in the same map. An extension of our own does not go on
   this rung.
3. **A new field number is genuinely required.** Then it belongs
   upstream in LXMF, proposed as such, and not shipped into one client
   ahead of that. A field number minted by us and understood by one app
   is a fork of the format with a friendlier name.

Two conditions apply at every rung. An extension must be **demonstrated
between two implementations that are both ours** before it is offered to
anyone else — what gets proposed is then a working feature rather than
an idea, and the cost of being wrong stays inside this project. And the
**failure mode on a viewer that does not participate must be written
down**: an extension whose effect on an old build has not been stated
has not been designed.

## Non-goals

- **A second format.** Not even for our own daemon-to-daemon path: one
  encoder, one decoder, one thing to get right.
- **Mirroring a viewer's internals.** Session bookkeeping and collector
  scheduling are each app's business. We match the wire, not the state
  machine — the same distinction [Python-RNS
  compatibility](python-rns-compatibility.md) draws between
  compatibility and parity.
- **Telemetry as a transport diagnostic.** What a node reports about
  itself is not how the mesh is measured; that is periculum's job and
  the status surfaces'.

## Checklist for a port, or for a new sensor

1. **Does every stamp come from the calendar clock of [Time and
   clocks](time-and-clocks.md) — best honest estimate, never ahead
   of it — and can the node say which arm anchored it?** No clock
   state blocks reporting; an unanswerable "where did this time come
   from" does block shipping.
2. **Does every sensor have a "no reading" state that omits its key?**
3. **Does an existing sensor ID fit?** Climb the ladder from rung one.
4. **Is the cadence stated as policy alone, with no airtime figure
   inside the telemetry module?**
5. **Are `content` and `title` empty on every reporting message?**
6. **Does the node announce a delivery destination with a name, and does
   it have a defined answer for a target it has never heard?** The
   defined answer is `awaiting-key` plus a path request, reported on the
   node's own status surface — not silence, and not a refusal to accept
   the target.
7. **If it collects for others: allow-list empty by default, set off
   the radio, four-element rows, inbound stamps clamped on ingest
   once the own calendar is healed (taken as-is before that), no
   future timestamps served?**
8. **What does a viewer that lacks this sensor show?** Answer before
   emitting.

Items 1 and 7 are test-bound beyond this page: the rule-by-tier
matrix in [Testing the model](time-and-clocks.md#testing-the-model)
names the cells. For a collector, the ingress-clamp row — clamp,
cursor safety, dedup, unhealed behaviour, Codeberg #239 — and the
authorship-interop row are the ones an implementation must land
green, with its implementing issue.
