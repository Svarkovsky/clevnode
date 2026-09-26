# Time and Clocks

How a node keeps calendar time, why it must always be able to, and how
a wrong calendar heals. This applies to every firmware and every
platform port, present and future. Issues come and go; this concept
stays.

This page is the binding spec of the anchor model. It replaces the
earlier doctrine "no verified clock, no authorship": the rule is now
that **every instance always authors, stamped with its best honest
estimate and never ahead of that estimate** — and that foreign
garbage timestamps must never break an instance. "Never ahead" is
exactly what the mechanisms deliver, no more: every fallback anchor
lies in the past, so an *uncertain* calendar errs backwards by
construction. It is not a claim to detect a wrong source: an anchor
that is wrong but inside the sanity window — including a plausible
*forward*-wrong one — is adopted and stamped as-is, a named residual,
time-bounded by the healing loop. The sections below define the
estimate, the anchor's provenance rank, the one filter every time
source passes, and the loop that heals a wrong calendar.
[Testing the model](#testing-the-model) closes the page: the
rule-by-tier matrix that binds every one of these rules to a named
test cell, Periculum included.

## The two poisons

Calendar failures hurt in exactly two directions, and they are not
symmetric.

**Too old, self-consistently.** A Reticulum peer orders
same-destination paths by the announce emission timestamp. Python-RNS
stamps `int(time.time())` — epoch seconds — into the announce random
hash (`reference/Reticulum/RNS/Destination.py:282`) and replaces a
stored path only when a new announce carries a *newer* emission than
the stored one (`announce_emitted > path_timebase`,
`reference/Reticulum/RNS/Transport.py:1772`; the worse-hop branch
runs the same newer-wins comparison against a separately computed
field, `announce_emitted > path_announce_emitted`,
`Transport.py:1809`). The value
we stamp is therefore compared on other machines, across our reboots,
against values our earlier selves emitted. A clock that is merely
self-consistent — process uptime — restarts from zero on every reboot
and loses that comparison forever: the node keeps announcing, and no
peer ever updates its path entry again (Codeberg #155). Obtaining
usable time is a protocol obligation, not a platform convenience.
#155 is one instance of a general failure class — a generated field
that is self-consistent between our writer and our reader and means
something else to a peer; the class and the audit method for it are
in [Wire Field Semantics](wire-field-semantics.md).

**Too far in the future.** The reverse error is worse, because it
poisons *others*, silently and permanently. Receivers advance
monotonic cursors over the stamps they ingest — the worked case is
the telemetry collector cursor ([Telemetry](telemetry.md)), which one
future-stamped row raises past every honest later reading, forever,
with no refusal logged anywhere. A past-stamped message, by contrast,
sorts too far back where it is displayed directly: visible,
attributable, self-limiting. One reader must be priced honestly
rather than folded into that: a collector serves only rows above a
requester's cursor, so a past-stamped row below an already-synced
cursor is not "sorted backwards" — it is *invisible* to that
requester, indistinguishable from loss, until the sender's calendar
heals (see [the one honest cost](#the-one-honest-cost)).

These two poisons shape the whole model. A node must always be able
to author — an instance that falls silent for lack of a clock fails
the switch-on-and-it-works requirement exactly when it is needed —
and when its calendar is uncertain, the error must land in the benign
direction: backwards, never forwards.

## Two clocks, strictly separated

A node runs two clocks with disjoint jobs, and no time correction
ever crosses from one to the other.

**The stopwatch** is the monotonic tick counter, `Clock::now_ms`
(`leviculum-core/src/traits.rs:349`). All timeout and deadline
arithmetic stays on it — retries, link timeouts, announce cadences.
No anchor change, GNSS fix, or healing step ever stretches or
shrinks a protocol timer.

**The calendar clock is an estimate, not a measurement:** an anchor
(unix seconds, obtained from some source) plus the stopwatch time
elapsed since that anchor was seated. Two properties follow by
construction:

- **It never stands still.** Between anchors it advances with the
  stopwatch, so repeated identical stamps — the #217 class of
  same-timestamp ID collisions — cannot come back.
- **It moves in jumps only when a better anchor re-seats it.** An
  anchor change is an explicit, recordable event with a source, not
  a drift.

The clockless arm of the implementation already has this shape: a
learned floor advanced by the monotonic clock, inside
`Transport::emission_secs` (`leviculum-core/src/transport.rs:3525`).
On std platforms the same estimate answers from the OS: the
`SystemClock` implementation of `wall_unix_secs`
(`leviculum-std/src/clock.rs:49`) reads `SystemTime`, with the
anchor-keeping delegated to the OS and its NTP discipline.

## One value, one producer

`Transport::emission_secs` (`leviculum-core/src/transport.rs:3525`)
is the single point that turns the calendar estimate into the
unix-seconds value for wire fields that peers compare across our
process lifetimes: announce emission timestamps, built by
`generate_random_hash` (`leviculum-core/src/announce.rs:156`), and
request timestamps
(`leviculum-core/src/node/mod.rs:1049`, `:1154`, Codeberg #164). Any
new wire field with cross-lifetime semantics draws from it too —
never from the monotonic `Clock::now_ms`, which is a timer, not a
calendar.

**Crates layered on `NodeCore` reach the same producer, they do not
take a parameter.** `NodeCore::emission_secs` exposes it; LXMF's three
cross-lifetime fields — the message timestamp, the ticket expiry and
the propagation upload timestamp — resolve through it inside the
router (`leviculum-lxmf/src/router.rs`, Codeberg #182). They used to
arrive as a `now_unix: f64` argument on `enqueue`, `tick`,
`handle_event` and `issue_ticket_field`, which is the #155 shape with
the defect moved into the caller: nothing about the signature stops a
clockless node from passing uptime seconds. An API that cannot be
called wrongly beats a doc comment warning about it. Pinned at
`leviculum-lxmf/tests/wall_clock_producer.rs`, structurally (no public
router entry point takes an `f64`) as well as by value.

### One producer, two resolutions

The field decides the resolution, not the clock. `emission_secs`
returns whole seconds for fields that are whole seconds on the wire —
the 5-byte announce emission timestamp above all. `emission_micros`
returns the same instant in microseconds, and
`NodeCore::emission_secs_f64` divides it back into fractional unix
seconds for fields that are floats. They are the same producer with
the same source ranking:
`emission_micros(now) / 1_000_000 == emission_secs(now)` on every arm.

The float resolution exists because LXMF hashes the message timestamp
into the message ID. At whole seconds, two identical messages created
inside one second are one ID, and the second is refused as a duplicate
of the first — a message the reference, writing `time.time()`
(`reference/LXMF/LXMF/LXMessage.py:357`), would have sent (Codeberg
#217).

**Microseconds, not milliseconds.** The collision is between two calls
in one code path, not two user actions. Two consecutive
`LxmfRouter::create_message` calls measure ~115 µs apart — each signs
an Ed25519 message — so a millisecond value collides on every such
pair; that was measured at 20 pairs out of 20 before this unit was
chosen. Microseconds is also what the reference effectively produces:
`time.time()` is an f64 of unix seconds, resolving to ~0.24 µs at
present-day timestamps.

On the clockless arm the sub-second part comes from our own monotonic
clock, and `Clock::now_ms` is the only monotonic source there. So
`emission_micros` returns `floor_secs * 1_000_000` plus the
*milliseconds* elapsed since the floor was set, scaled up: monotonic,
separating two instants one millisecond apart, and honest about the
resolution the platform actually has. It is not a claim to know the
wall-clock microsecond, and nothing reads it as one. Platforms whose
`Clock` implements only `wall_unix_secs` get the trait default —
`secs * 1_000_000` — and keep working unchanged, with no precision to
gain and none invented.

### The one refusal left: a field the peer discards in silence

Authorship is never refused — that is the headline rule of this page.
One narrow refusal survives it, and it blocks no message.

The LXMF ticket expiry is compared on the *peer's* clock: a peer
keeps a ticket only while `time.time() < expires` on its own machine
(`reference/LXMF/LXMF/LXMRouter.py:1854`) and says nothing when it
does not. A backwards-biased expiry from an unhealed calendar is
therefore already expired on arrival — issuing it is emitting a field
the peer silently discards. `LxmfRouter::issue_ticket_field`
(`leviculum-lxmf/src/router.rs:640`) returns
`RouterError::NoWallClock` while the calendar is not a plausible wall
clock, rather than issue one: a named error is a diagnosis; a
discarded ticket is a mystery that surfaces months later as "replies
from this peer are slow".

"Plausible" here is a question about the anchor's
[provenance rank](#anchor-provenance-is-first-class-state), not
about its value: a birth-anchored (rank 5) calendar holds a value
that passes the sanity window and is refused anyway, because its
estimate is recognisably behind real time and every expiry it
computes is already in the past on every healed peer. Today the gate
is the value test `NodeCore::has_plausible_wall_clock`
(`leviculum-core/src/node/mod.rs:3077`), which is correct only until
a port plumbs its build timestamp — the rank section says why, and
binds the fix to the same change.

Fields the peer decides nothing on are always emitted. The LXMF
message timestamp (displayed and sorted, `LXMessage.py:357`
unvalidated) and the propagation upload timestamp (bound and dropped,
`LXMRouter.py:2238-2240`) both flow regardless of clock state — a
backwards-biased stamp mis-sorts, and mis-sorting is the accepted
cost, not a failure. Withholding a message because our clock is
uncertain would be the far worse failure.

The refusal is always on *writing*, never on reading: a peer's ticket
is remembered and used regardless of the state of our own clock.

## The sanity window

Every candidate anchor, from every source — RTC, GNSS, host
injection, network-learned time — passes one filter. No source gets a
special case, and no source bypasses it.

- **Lower bound: the build timestamp.** The firmware or binary build
  time is compiled in: free, always present, and incorruptible by any
  runtime input. Real time is always after it, so any source claiming
  a moment before it — a 1999 RTC with a dead backup cell — is
  deterministically garbage, not merely suspicious.
- **Upper bound: a generous margin above the best known anchor**, on
  the order of decades above the build floor. It only has to separate
  values a real clock could hold from values none can; its exact size
  is a tunable practice parameter (see
  [Practice parameters](#practice-parameters-not-dogma)), not dogma.

A source that fails the window is refused as an anchor — never
"corrected" — and the calendar keeps running on the best anchor it
has. GNSS gets no bypass: it is simply the highest-ranked source
inside the same filter, trusted by default and rejected when
implausible. A receiver subtly shifted *within* the window (a spoofed
or faulty fix that still looks plausible) is an accepted residual
risk, time-bounded by the [healing loop](#the-healing-loop).

The window answers exactly one question: may this *value* seat an
anchor at all. Every other predicate in the model — is first
adoption still unbounded, may a ticket be issued, does the calendar
count as a plausible wall clock — keys on the anchor's provenance
rank, not on its value clearing the window. The next section is that
rule; skipping it re-introduces two regressions by accident.

**Implementation status.** Today the window is two fixed-date
constants: the lower bound `EMISSION_PLAUSIBLE_MIN_SECS`
(`constants.rs:539`, 2020) and the upper bound
`EMISSION_LEARN_CEILING_SECS` (`constants.rs:521`, 2200-01-01),
enforced on learning and host injection. They approximate the binding
bounds with constants that need no build plumbing; deriving the floor
from the build timestamp was already named as the tightening in #161
§1 and is spec under this model. Until a port plumbs its build
timestamp through, the fixed dates stand in.

## Anchor provenance is first-class state

An anchor is a pair: the value it seated and the **rank** of the
source that seated it (the arms of the next section). The calendar
keeps both, and the model's predicates split cleanly over the two:

- **The sanity window bounds anchor *admission*** — a question about
  the value, asked once, on the way in.
- **Adoption, healing and ticket predicates key on the anchor's
  *rank*** — never on whether its value happens to clear the
  plausibility floor.

The distinction is load-bearing, because the build floor (arm 5)
sits *at* the plausibility floor by construction: every firmware is
built after 2020, so a birth anchor passes every value test from the
moment arm 5 is plumbed. Two predicates in the tree then misfire if
they stay keyed on the value:

- **Unbounded first adoption.** `learn_emission_timebase`
  (`leviculum-core/src/transport.rs:3669`) selects the unbounded
  branch today by `current < EMISSION_PLAUSIBLE_MIN_SECS`
  (`transport.rs:3425`). A birth-anchored cold node clears that
  test, so its first credible announce would fall into the *bounded*
  branch and the node would crawl to real time at one day per
  announce — the exact #161 §1 regression this page forbids —
  instead of healing in one step.
- **The ticket refusal.** `NodeCore::has_plausible_wall_clock`
  (`leviculum-core/src/node/mod.rs:3077`) becomes vacuously true at
  the build floor: the refusal never fires, and a birth-anchored
  node issues tickets whose expiry is already in the past on every
  healed peer — the silently-discarded field the refusal exists to
  prevent.

The binding rule: **while the calendar is anchored at rank 5
(birth), first adoption is unbounded, tickets are refused, and the
calendar does not count as a plausible wall clock — regardless of
the anchor's value.** Stated the other way round: a build-floor
anchor passes the sanity window for *stamping* — the node authors,
per the stamping rule below — but it is never "plausible" for
tickets or for capping adoption.

The value predicates in the tree are correct today only because no
port has plumbed its build timestamp yet: the sourceless states they
actually see (uptime seconds) do sit below the floor. The issue that
lands the build floor must land the rank switch in the same change,
or it lands both regressions with it.

## The source ranking

The calendar takes its anchor from the best source available, and a
higher-ranked source re-seats an anchor from a lower-ranked one — the
arms 1 to 4 of the old chain survive here as ranks. The order is by
how hard the source is to fool, not by precision: a GNSS fix is a
live measurement, a host injection is an explicit claim by an
operator, an RTC is whatever it was last set to, and network-learned
time is arbitrary input from anyone in radio range. Every arm passes
the same [sanity window](#the-sanity-window). For each: what it
costs, when it is unavailable, what it guarantees.

> **Rustdoc debt.** Two doc comments in the tree state a different
> order and must be updated by the issue that implements this
> ranking: the rustdoc of `set_wall_time_unix_secs`
> (`leviculum-core/src/node/mod.rs:734`, and on the transport at
> `transport.rs:3362`) says a platform wall clock always takes
> precedence over an injection — the reverse of arms 2 and 3 — and
> the rustdoc of `NodeCore::emission_secs`
> (`leviculum-core/src/node/mod.rs:2479`) lists the chain as
> "platform wall clock, learned announce timebase, host injection,
> uptime". This page is the spec; those comments are the debt.

### Arm 1: GNSS

Where the board has a receiver — today the WisMesh Pocket V2's
u-blox ZOE-M8Q (`leviculum-nrf/src/gnss.rs`). The NMEA RMC sentence
carries UTC date and time in every fix.

- **Cost:** receiver power, a sky view, and cold-start acquisition
  time (seconds to minutes).
- **Unavailable:** indoors or shadowed — a node without sky view
  never gets a fix, so GNSS *seeds* the calendar, it never replaces
  the ranking below it.
- **Guarantees:** UTC to well under a second, far beyond the
  one-second wire granularity. Trusted by default; a fix outside the
  sanity window is refused like any other source.
- **Status:** the firmware parses RMC but consumes only position and
  validity (`GnssFix`, `leviculum-nrf/src/baseboard.rs:26`, has no
  time field yet). Seeding the calendar from RMC is tracked as a
  firmware implementation issue, not here. See
  [GNSS specifics](#gnss-specifics-for-a-board-bring-up) below.

### Arm 2: Host injection

`Node::set_wall_time_unix_secs` (`leviculum-core/src/node/mod.rs:833`
→ `transport.rs:3362`), for deployments where a clockless node has a
host that does know wall time — e.g. a control frame on the LNode
serial channel (the radio-config envelope of
`leviculum-core/src/rnode.rs`).

- **Cost:** one control-channel frame; requires a host that itself
  has a trustworthy clock.
- **Unavailable:** standalone nodes with no host attached.
- **Guarantees:** host-clock quality, sanity-gated: values outside
  `[EMISSION_PLAUSIBLE_MIN_SECS, EMISSION_LEARN_CEILING_SECS]`
  are refused (`transport.rs:3371`), because an injection *claims*
  to know wall time, so a value no real clock can hold is
  self-refuting. Pinned at
  `test_implausibly_low_wall_time_injection_is_refused`
  (`transport.rs:18958`) and
  `test_absurd_wall_time_injection_is_refused`
  (`transport.rs:19173`).
- **Status:** wired (#238): the control envelope's wall-time frame
  (`docs/src/firmware/usb-control-envelope.md`) carries a u64 of unix
  seconds from the host to the seam; the seam's bool picks the
  enveloped ack or the named `value refused` answer, an accepted seed
  logs `[TIME_SEED] source=host`, and `lnflash --set-time` is the
  speaker.

### Arm 3: Platform clock passing sanity

`Clock::wall_unix_secs` (`leviculum-core/src/traits.rs:368`). On std
platforms `SystemClock` answers from `SystemTime`
(`leviculum-std/src/clock.rs:50`) — effectively the OS's NTP-managed
clock. On a board it is a battery-backed RTC, which under this model
also carries anchors *back*: see
[the healing loop](#the-healing-loop) for the write-back.

- **Cost:** none.
- **Unavailable:** on MCUs without an RTC. The LNode's
  `EmbassyClock` (`leviculum-nrf/src/clock.rs`) keeps the trait
  default of `None` — that is correct, not a gap: returning
  uptime-derived values from `wall_unix_secs` would be lying to the
  transport.
- **Guarantees:** whatever the platform clock guarantees — NTP
  quality on a host, last-set-plus-drift on an RTC. An RTC counts
  only when its value passes the sanity window; below the build
  floor it is a dead cell, not a time source. Note the trait
  contract: this is *not* a timer source; all timeout and deadline
  arithmetic stays on the monotonic `now_ms` (`traits.rs:349`).
- **Status:** the implementation consults this arm first when it
  answers (`transport.rs:3263`). No current platform offers both a
  platform clock and GNSS or injection, so the difference in order
  has no behavioural effect today; a port that has both follows this
  ranking.

### Arm 4: Network-learned

The sourceless fallback: `learn_emission_timebase`
(`transport.rs:3669`) adopts the highest emission timestamp seen in
any signature-valid announce as the calendar anchor, then advances it
with the monotonic clock (`transport.rs:3268`). This includes the
node's *own* pre-restart announce echoing back from a neighbour —
learning deliberately runs before the own-destination echo drop, so a
rebooted node re-seeds past exactly the value its next announce must
exceed — pinned at
`test_own_announce_echo_reseeds_timebase_before_echo_drop`
(`transport.rs:18765`).

- **Cost:** nothing — no hardware, no host.
- **Unavailable:** on a mesh where no participant has a clock, or
  before the first plausible traffic arrives.
- **Guarantees:** only as good as radio-range neighbours, and
  `validate()` proves only that the announce signs itself — the
  field is arbitrary input from anyone in range. Hence every
  hardening rule below and the re-anchor rules of the
  [healing loop](#the-healing-loop). A calendar seated by this arm
  is **anchored from traffic, unconfirmed**: the evidence may itself
  be another unhealed node's birth clock (see
  [the one honest cost](#the-one-honest-cost)). The learned anchor
  is in-memory only on RTC-less boards: after a reboot the node
  starts from the build floor again until live traffic re-seeds it.

### Arm 5: The build floor

The birth state of every instance with no better source: the
calendar anchors at the build timestamp, advanced by uptime. This
replaces raw uptime seconds as the bottom of the ranking, and it is a
*valid* state, not a defect: the stamp is recognisably old, but
unique, monotonic, and non-toxic — it can never poison a peer's
cursor, because it errs backwards. Nobody stays silent, nobody
poisons; the cost is confined to
[the one honest cost](#the-one-honest-cost) below. The rank is the
part that matters beyond the value: a rank-5 anchor stamps, but it
never counts as a plausible wall clock, never issues tickets, and
never caps adoption
([provenance rank](#anchor-provenance-is-first-class-state)).

- **Cost:** none; the build timestamp is compiled in.
- **Unavailable:** never — that is the point.
- **Guarantees:** uniqueness and monotonicity within the boot, a
  value that is always in the past, and a floor the sanity window
  can trust.
- **Status:** spec. Today an instance with no source emits raw
  uptime seconds (`transport.rs:3270`), which every cross-restart
  comparison loses (#155) and which the old doctrine treated as a
  defect to declare. Plumbing the build timestamp into each port
  retires that state entirely.

## Stamping: always author, never ahead of the estimate

Every instance always stamps outgoing fields with its best honest
estimate of now — the calendar clock, whatever its anchor. The
directional guarantee is stated at its real strength: **we never
stamp ahead of our own estimate, and every fallback anchor lies in
the past**, so an *uncertain* calendar errs backwards by
construction. That is where the safety comes from: the poison was
never wrong time but *future* time (the cursor mechanism in
[the two poisons](#the-two-poisons) is why — permanent, silent,
hurts others), while past time hurts only presentation. What the
mechanisms do **not** deliver is detection of a wrong source: an
anchor that is wrong but inside the sanity window — a plausible
forward-shifted GNSS fix, a mis-set host clock — is adopted and
stamped as-is. That residual is named here rather than implied away,
and it is time-bounded by the [healing loop](#the-healing-loop). A
source-less instance anchors at build-time plus uptime and authors
anyway.

The old rule — no verified clock, no LXMF authorship — is withdrawn.
It bought cursor safety by silencing exactly the instances a mesh is
for, and the same safety is now had cheaper: backwards-biased
stamping at the writer, [ingress clamping](#ingress-clamp-for-semantics-keep-for-display)
at the reader.

### The one honest cost

Between cold start and healing, outgoing messages carry recognisably
old stamps, and the cost is priced per path. Where a stamp is
displayed directly, it sorts backwards — a Sideband conversation
shows messages out of order until the calendar heals: visible,
attributable. On the collector path the same stamp costs more, and
the real price is stated rather than rounded down: a collector
serves only rows above a requester's cursor, so a birth-stamped row
sits *below* every already-synced requester's cursor — not sorted
backwards but **invisible to that requester, indistinguishable from
loss**, until the tracker heals and stamps climb past the cursor
([Telemetry](telemetry.md)).

How the cost ends is graded by what healed the calendar:

- **Contact with arms-1–3-quality time** — a GNSS fix, a host
  injection, a plausible platform clock — ends it outright.
- **Traffic healing (arm 4) ends it provisionally.** A foreign stamp
  that passes our sanity window may itself be another unhealed
  node's birth clock: a build-floor stamp from a node built after us
  (or comparably) clears our own floor. The calendar is then
  **anchored from traffic, unconfirmed** — better than birth, not
  yet known-good — and stays in that state until arms-1–3-quality
  contact confirms or corrects it. Two clockless nodes healing from
  each other are an echo chamber, and nothing on the wire can fully
  prevent it: the stamps are signature-valid and in-window. A named
  residual, not a solved problem.

We state the cost rather than hide it, because the alternatives are
the two poisons: silence or invented time.

## Ingress: clamp for semantics, keep for display

The mirror image of backwards-biased stamping protects us from
everyone else. An incoming stamp beyond the local plausible-now is
**clamped to receive time for ordering and cursor semantics**:
indexing and above all cursor advancement — in our collector (#239)
and in any future propagation node. The original stamp is kept
alongside as local display information, so nothing is destroyed and
a viewer on this node can still show what the sender claimed.

**Local plausible-now, defined.** The basis is the local calendar
estimate at receive time — `Transport::emission_secs`
(`leviculum-core/src/transport.rs:3525`) — plus a bounded forward
tolerance for honest clock skew between sender and receiver. A stamp
at or below basis-plus-tolerance passes as-is; above it, it is
clamped to receive time. The tolerance is a practice parameter (see
[Practice parameters](#practice-parameters-not-dogma)): its job is
to keep two honestly-synced clocks from clamping each other, not to
admit the future.

Three boundaries keep the clamp from doing damage of its own:

- **Dedup keys are not clamped.** Deduplication runs on content or
  transient ID, never on the timestamp, so clamping can neither
  make two distinct readings collide nor let a replay through. The
  clamp covers ordering and cursor semantics only.
- **What is served is the clamped value.** A telemetry stream row
  has exactly one timestamp slot (the row form in
  [Telemetry](telemetry.md)), and a collector serves what it
  indexed. Keep-for-display is local-only — and the limit of the
  defence is stated with it: the row's `packed_telemetry` payload
  still carries the sender's raw claim in its own `SID_TIME`, so a
  downstream reader that parses the payload sees the claim. Cursors
  — ours and every requester's — advance over the clamped value
  regardless.
- **The clamp is armed only by a healed calendar.** Clamping "to
  receive time" presumes the receiver knows what time it is. An
  unhealed collector — birth-anchored, or below arms-1–3 quality
  with no traffic re-anchor yet — would clamp every honest current
  stamp down to its own ancient notion of now and blackhole the
  mesh's telemetry into a years-old index. While unhealed, it takes
  in-window sender stamps as-is; those same stamps are
  simultaneously its healing evidence (arm 4). Rows ingested before
  healing keep their index stamps — there is no re-index — and that
  cost is part of the honest cold-start story above.

The reason for the clamp is the cursor mechanism: a cursor that
advances over a raw foreign stamp hands every sender a lever to
starve it. Clamped, the worst a garbage stamp can do is index as
"arrived now" — wrong by presentation, harmless by mechanism.
Foreign garbage can no longer break us, which is the other half of
the always-author rule: authorship without ingress protection would
just move the poison one hop.

Scope: this clamp lives at the application layer — what we index,
serve, and advance cursors over. It does **not** touch announce path
ordering, which stays raw for reference parity; see the
non-behaviours below.

## Rules that hold regardless of source

### The wire field is 40 bits; every producer saturates

The announce timestamp field holds
`8 * RANDOM_HASH_TIMESTAMP_SIZE = 40` bits. A larger value would
silently drop its high bits on the wire and sort *below* every stored
path entry — the node instantly loses path replacement everywhere.
`EMISSION_TIMESTAMP_MAX_SECS`
(`leviculum-core/src/constants.rs:512`) caps it, enforced at the
point of resolution (`transport.rs:3278`) and again at the wire
producer (`announce.rs:167`), so truncation is unrepresentable
regardless of which source produced the value. Incident: Codeberg
#160. Pinned at `test_emission_secs_saturates_at_wire_field_max`
(`transport.rs:19197`).

### The timebase never moves backwards, and adoption is windowed

Within arm 4, an older emission never regresses the anchor
(`emitted_secs <= current`, `transport.rs:3414`), and adoption is
bounded by the sanity window: values above
`EMISSION_LEARN_CEILING_SECS` (`constants.rs:521`, 2200-01-01) cannot
come from a real clock and are refused outright
(`transport.rs:3410`); the lower bound `EMISSION_PLAUSIBLE_MIN_SECS`
(`constants.rs:539`, 2020) separates real wall clocks from
uptime-derived values, which sit orders of magnitude apart.
Incidents: #160, #161. Pinned at
`test_clockless_node_learns_emission_timebase_from_announce`
(`transport.rs:18668`) and
`test_timebase_floor_cannot_pass_learn_ceiling`
(`transport.rs:19113`).
The per-announce no-backwards guard is not contradicted by the
healing loop: a backwards re-anchor is a deliberate event that
requires arms-1–2 evidence and respects the emitted high-water mark
(see [the healing loop](#the-healing-loop)) — never one announce
dragging the anchor down.

### The FIRST adoption is unbounded while anchored at rank 5

While the calendar is still on its birth anchor — rank 5, or one of
today's stand-in states below the value floor — adoption is
deliberately unbounded (`transport.rs:3425`): a node starting at the
build floor, or one that adopted a rebooting peer's uptime seconds
as its first anchor, must climb to real unix time in one step.
First-plausible-wins is correct *here*, and only here: a birth
anchor has nothing worth defending, and instant recovery beats
attack resistance for it. The predicate is the anchor's **rank**,
not its value: a birth anchor's value clears the plausibility floor
once the build floor is plumbed, and a value test would then route a
cold node into the bounded branch below
([provenance rank](#anchor-provenance-is-first-class-state)).
**Capping this was a real regression** (#161 §1): with the bounded
advance applied to an implausibly low anchor, recovery crawled at
one day per announce — about 20 602 announces, ~429 days at a
30-minute LoRa cadence — where a single credible announce used to
recover the node instantly. Do not re-introduce that cap. The
no-backwards guard above keeps the unbounded branch from being
abused downwards. Pinned at
`test_clockless_first_timebase_adoption_is_unbounded`
(`transport.rs:18855`) and
`test_clockless_timebase_advance_is_bounded_after_first_adoption`
(`transport.rs:18806`).

### Advance past rank 5 is bounded — per announce, not per peer

Once the calendar is no longer birth-anchored (today: once the value
clears the plausibility floor), one announce may advance it by at
most `EMISSION_LEARN_MAX_ADVANCE_SECS` (`constants.rs:539`, one
day), so a peer whose clock is decades wrong cannot capture the
calendar in one announce. State the protection level honestly:
learning runs before the per-destination announce rate limit and the
rebroadcast dedup, so *N announces advance the floor by N × cap*
regardless of how many identities or destinations they came from.
The real cap on the walk rate is announces-per-second on the air —
nothing identity-shaped. This measured reality is pinned at
`test_timebase_walk_is_capped_per_announce_not_per_identity`
(`transport.rs:19039`); the walk terminates at the learn ceiling,
pinned at `test_timebase_floor_cannot_pass_learn_ceiling`
(`transport.rs:19113`). No durable defence is claimed from the
[healing loop](#the-healing-loop)'s median: a median over free
identities resists a broken peer, not an attacker. What actually
bounds a hostile forward walk is the ceiling and the airtime it
costs; what undoes one afterwards is arms-1–2 evidence, because the
healing loop deliberately refuses to move a calendar *backwards* on
traffic alone.

### We do not validate our own clock, or incoming timestamps

Two deliberate non-behaviours, both reference parity. The anchor
model does not soften them — it validates *anchors* on the way into
the calendar, never emissions on the way out or announces on the way
into the path table:

- **Our own calendar estimate is emitted verbatim.** Python fills the
  field from `time.time()` unvalidated (`Destination.py:282`);
  bounding, substituting, or withholding our value at emission time
  would desynchronise us from a network that does not validate.
  Under the anchor model an "implausible own clock" collapses to "no
  anchor better than the build floor" — and that state emits too,
  per the stamping rule. The once-per-process operator warning in
  `Transport::announce_emission_secs` (`transport.rs:3598`) remains
  the only reaction to an implausible value — never an altered
  emission. Pinned at
  `test_own_wall_clock_is_not_plausibility_bounded_on_emission`
  (`transport.rs:19358`) and
  `test_implausible_own_wall_clock_warns_once_and_leaves_emission_unchanged`
  (`transport.rs:19406`).
- **Incoming emission timestamps are not plausibility-checked on
  path acceptance.** Ordering is per-destination comparison only,
  exactly `announce_emitted > path_timebase`
  (`Transport.py:1772`; the worse-hop branch compares against its
  own stored field at `Transport.py:1809`). A clockless peer's
  uptime-seconds announce must enter the path table (that is how a
  #155 node is reachable at all), and an absurdly high emission must
  win the newer-emission comparison. Python peers accept both;
  filtering would only desynchronise our path tables from every
  other node's view of the same announces. Pinned at
  `test_incoming_emission_not_plausibility_checked_on_acceptance`
  (`transport.rs:19477`). The
  [ingress clamp](#ingress-clamp-for-semantics-keep-for-display)
  operates strictly above this layer — on what we index and serve,
  never on what we route.

The sanity window exists for *anchor adoption* — learning, host
injection, GNSS, RTC — never for emission or path acceptance.

## The healing loop

The protocol self-heals wherever the evidence for healing exists;
where it does not, this section names the residual instead of
claiming one. The loop, binding as spec:

1. **Collect.** Plausible foreign times are gathered from live
   traffic the node already receives — LXMF message stamps,
   propagation announces. Plausible means: passes the sanity window.
2. **Re-anchor — by rank and by direction.**
   - **While anchored at rank 5, a single plausible source
     re-anchors the calendar.** This *is* the unbounded first
     adoption above, restated as the healing rule: a birth anchor
     has nothing worth defending. Unless the source was arms 1–3,
     the result is
     [anchored from traffic, unconfirmed](#the-one-honest-cost).
   - **A calendar already anchored to real time is never re-anchored
     by a single sender.** A *forward* correction requires gross
     deviation from the **median of several distinct senders**;
     cohort size and deviation threshold are practice parameters.
   - **A gross *backwards* correction additionally requires arms-1–2
     evidence — a GNSS fix or a host injection — never traffic
     alone.** Announce identities are free (this page already
     concedes that for the walk cap), so a traffic median in the
     past is exactly what an attacker can fabricate; a backwards
     path open to traffic would be a remote lever for dragging any
     healed calendar down and silencing its announces. Closing it
     costs a residual, named under "sparse meshes" below.
   - **The emitted high-water rule, binding on every re-anchor:**
     the calendar is never re-anchored below the highest value this
     identity has ever emitted — not even by arms-1–2 evidence; the
     correction floors at the high-water mark. Peers order our
     announces by emission timestamp (`announce_emitted >
     path_timebase`, `reference/Reticulum/RNS/Transport.py:1772`),
     so dropping below our own emitted high-water silences our
     announces mesh-wide until the calendar climbs past it again —
     and re-stamping a range we already stamped would revive the
     #217 same-stamp class this page claims cannot come back. Where
     storage exists, the high-water mark is persisted across boots.
     An RTC-less, storage-less node cannot persist it, and that
     cost is named: such a node re-enters the same
     build-plus-uptime stamp range on every boot until re-seeded.
     The practical mitigation is already in the model — arm 4 hears
     the node's *own* pre-restart announces echo back and re-seeds
     past them, pinned at
     `test_own_announce_echo_reseeds_timebase_before_echo_drop`
     (`transport.rs:18765`).
3. **Write back to the RTC.** Every better anchor is also written
   into a present RTC, so the hardware clock itself heals and the
   next boot starts from the healed value instead of the build
   floor. The write-back is what turns arm 3 from
   "whatever it was last set to" into "whatever we last verified".

**What the median is, honestly.** A median over distinct senders
resists a *broken peer*: one wrong clock in an honest cohort cannot
move it. It does not resist an *attacker* — identities are free, and
a cohort of them is one attacker with a loop. The attack-facing
guarantees come from the other rules: the ceiling and the advance
cap bound a forward walk, the arms-1–2 requirement closes the
backwards lever, and the high-water rule caps what any accepted
correction may do to our own emissions.

**Sparse meshes, honestly.** With one neighbour no cohort exists, so
a calendar that is *grossly forward* — and no longer at rank 5 —
does not heal from traffic at all: the median that would justify a
correction cannot form, and traffic alone may never pull backwards
anyway. The residual is stated rather than papered over: such a node
heals through a GNSS fix, a host injection, or a reflash — not from
listening. Until then it keeps authoring, and the learn ceiling
keeps it from walking further.

The cold-start story then reads: a device with nothing stamps from
its birth anchor; the first plausible contact re-anchors it — to
known-good time when the contact was arms 1–3, to
traffic-unconfirmed when it was a foreign stamp, which may itself be
another unhealed node's birth clock (the echo-chamber residual of
[the one honest cost](#the-one-honest-cost)); the RTC (where
present) keeps it across power cycles; and a calendar later walked
wrong is corrected forward by its cohort's median, backwards only on
arms-1–2 evidence. No operator action at any step — switch on and it
works, with the residuals stated.

**Implementation status.** Arm 4's single-announce learning
(`learn_emission_timebase`, `transport.rs:3669`) implements the
rank-5 re-anchor today. The median re-anchor, the collection of
LXMF-stamp evidence, the high-water persistence, and the RTC
write-back are spec, tracked as implementation issues per platform.

## GNSS specifics for a board bring-up

- **Use NMEA UTC, never raw GPS time.** GPS system time does not
  observe leap seconds and is currently 18 s ahead of UTC. The
  receiver applies the broadcast UTC offset before it builds the RMC
  sentence, so RMC date + time *is* UTC — take it from there.
  Getting this wrong is silent: an 18-second skew breaks nothing
  visibly and is indistinguishable from clock drift in the field.
- **Acquire, seed, let the receiver sleep.** One fix seeds the
  calendar; the monotonic clock carries it forward. Crystal drift
  (tens of ppm — under half an hour per year of isolation) is
  irrelevant at one-second wire granularity. Keeping the receiver
  powered buys nothing for time.
- **Seed through the sanity window.** Route the fix through the same
  filter as every other source so a garbage fix cannot wedge the
  calendar; a valid RMC should pass it trivially. A subtly shifted
  fix inside the window is the accepted residual risk named above,
  time-bounded by the healing loop.
- **GNSS never replaces the ranking.** A node without sky view never
  gets a fix. Arms 2–5 must behave exactly as if no receiver were
  fitted.

## Record the source

A node should be able to state, at any moment, where its notion of
time came from: GNSS, host injection, platform clock, learned from
traffic (whose, and when, and whether still
[unconfirmed](#the-one-honest-cost)), a median re-anchor (over which
cohort), or the build floor. This is more than a breadcrumb: the
anchor's rank is live state the model's predicates key on
([provenance rank](#anchor-provenance-is-first-class-state)), so a
node that cannot answer it cannot even decide whether it may issue a
ticket. Diagnosis of a path-ordering problem starts with
"what did this node think the time was, and who told it" — without
provenance, a wrong timestamp in a peer's path table cannot be
attributed to a dead RTC, a lying neighbour, or a boot-order race.
The healing loop raises the stakes: a re-anchor is a calendar jump,
and an unattributed jump is indistinguishable from a bug. Today the
only breadcrumb is the once-per-process implausible-own-clock warning
(`transport.rs:3336`); exposing the current source and its origin
(status RPC, control-channel query) is part of implementing this
concept on each platform.

## Practice parameters, not dogma

The model fixes mechanisms; these values tune them. Each is a
practice parameter: chosen to work, changed by measurement and a
reasoned commit, never load-bearing for the model itself.

- **The upper sanity margin** above the best known anchor — order of
  decades; today the fixed date in `EMISSION_LEARN_CEILING_SECS`
  (`constants.rs:521`).
- **The lower bound stand-in** `EMISSION_PLAUSIBLE_MIN_SECS`
  (`constants.rs:539`) until build-timestamp plumbing retires it.
- **The per-announce advance cap**
  `EMISSION_LEARN_MAX_ADVANCE_SECS` (`constants.rs:539`).
- **The healing cohort**: how many distinct senders form a median,
  and how large a deviation counts as gross.
- **The local plausible-now tolerance**: the bounded forward skew
  allowance added to the local calendar estimate at the
  [ingress clamp](#ingress-clamp-for-semantics-keep-for-display) —
  basis is `Transport::emission_secs` at receive time; the tolerance
  absorbs honest sender/receiver skew, nothing more.

## Testing the model

Every binding rule on this page maps to a named test cell in a
named tier, and this section is that map. It is a concept-level
test contract: **each implementing issue lands with its cells from
this matrix, in the same change, never as a follow-up**, and a
reviewer ticks the touched rules off against this section before
merge. Cells that pin behaviour already implemented are cited;
cells for spec-only mechanisms are named here and land with the
issue that implements them — for those, named-but-not-landed is
the intended state until the issue lands, not accumulated debt.
This page names cells; it writes no scenario files and no test
code — the implementing issues fill the names in.

### The tiers

Four tiers, in the order a failure should be caught:

- **unit / mvr.** Deterministic, one to two nodes, under five
  seconds, structured event logs from all sides — the mvr
  constraints of the project's protocol-debugging discipline.
  Canonical mvr home: `leviculum-std/tests/mvr/`; unit pins live
  beside the code they pin. Everything decidable on one node's
  state machine is decided here.
- **workspace integration.** The workspace test suite: multi-crate
  seams, restart persistence, structural API pins.
- **periculum conformance.** Docker, against real Python-RNS,
  under the drop-in discipline: the SAME driver (`lns selftest`,
  `rnstatus`, the same client binary) pointed at `lnsd` and `rnsd`
  — never parallel per-stack drivers, which smuggle config
  differences into what claims to be a stack comparison. The
  project's interop rule applies in full: every cross-stack rule
  gets a positive AND a negative cell against real Python.
- **periculum hardware.** The rig, and only where the radio or a
  real peripheral — a GNSS receiver, an RTC — is itself the
  subject. Time logic decidable without hardware is decided in a
  lower tier; this tier verifies the peripheral wiring, not the
  model.

### The matrix

A dash means the rule has no cell in that tier by design, not that
one is missing. Names not yet in the tree are the binding intent;
the implementing issue may adjust a file name, but it updates this
matrix in the same commit.

| # | Rule | unit / mvr | integration | conformance | hardware |
|---|------|------------|-------------|-------------|----------|
| 1 | Stopwatch/calendar separation | `calendar_jump_timer_isolation` | — | — | — |
| 2 | Sanity window, per source | window trio per arm (arms 2 and 4 pinned) | — | — | — |
| 3 | Anchor rank predicates | rank-5 pair + build-floor negative | `wall_clock_producer.rs` | — | — |
| 4 | Stamping asymmetry | never-ahead property + monotonic pair | — | — | — |
| 5 | Emitted high-water | re-anchor floor | restart persistence | `time_high_water_path_retention` | — |
| 6 | Healing loop | median / single-sender / backwards / Sybil / RTC write-back | — | — | — |
| 7 | Echo chamber | — | — | `time_echo_chamber_third_peer` | — |
| 8 | Ingress clamp + collector | clamp / dedup / unarmed | — | `time_collector_clamp_cursor`, `time_collector_unhealed_heals` | — |
| 9 | Authorship interop | — | — | `time_cold_authorship_python_receiver`, `time_python_future_stamps` | — |
| 10 | Peripheral seeding and write-back | — | — | — | `time_gnss_seed_rig`, `time_rtc_writeback_rig` |

### The cells, rule by rule

**1. Stopwatch/calendar separation**
([Two clocks](#two-clocks-strictly-separated)). mvr
`calendar_jump_timer_isolation`: a wall-time injection lands in
the middle of a live link, in both directions — a decades-forward
jump and a backwards re-seat — and no relative timer moves. Retry
cadence, keepalive interval and path expiry are observed unchanged
across the jump on the structured event timeline; the test fails
if any timer stretches or shrinks. This is the separation rule's
negative cell and its only cell: the rule forbids exactly one
thing, and this watches for it.

**2. Sanity window, per source**
([The sanity window](#the-sanity-window)). One unit trio per
source arm: a below-build-floor value is refused (the 1999-RTC
dead-cell shape), an above-margin value is refused (a 2500-RTC),
an in-window value is accepted. Arm 2 is pinned today at
`test_implausibly_low_wall_time_injection_is_refused`
(`transport.rs:18958`) and
`test_absurd_wall_time_injection_is_refused`
(`transport.rs:19173`); arm 4's ceiling at
`test_timebase_floor_cannot_pass_learn_ceiling`
(`transport.rs:19113`). The arm-1 trio lands with GNSS seeding and
carries the no-bypass assertion: GNSS passes the SAME filter, and
the cell is written against the shared filter path so that it
would go red if a GNSS special case were ever introduced — an
out-of-window fix must be refused by the same code that refuses an
out-of-window injection.

**3. Anchor rank predicates**
([provenance rank](#anchor-provenance-is-first-class-state)).
Unit, landing with the build-floor plumbing: at rank 5 the
unbounded first adoption fires AND tickets are refused; after an
arms-1–3 anchor the per-announce adoption cap binds AND tickets
are issued; and the negative that catches the value-test misfire —
the build-floor value alone never satisfies the plausibility
predicate, even though it clears the sanity window. The value-test
stand-ins are pinned today at
`test_clockless_first_timebase_adoption_is_unbounded`
(`transport.rs:18855`) and
`test_clockless_timebase_advance_is_bounded_after_first_adoption`
(`transport.rs:18806`); the rank-keyed cells replace their
predicates without loosening their assertions. Integration:
`leviculum-lxmf/tests/wall_clock_producer.rs` remains the
structural pin that no router entry point takes a caller-supplied
wall clock.

**4. Stamping asymmetry**
([Stamping](#stamping-always-author-never-ahead-of-the-estimate)).
Unit: clockless stamps are exactly build-floor plus uptime;
consecutive messages are strictly monotonic (the #217 class); and
the directional guarantee is checked property-style over a
sequence of anchor changes — adoption, re-anchor, saturation —
asserting after every step that the stamp never runs ahead of the
estimate and never repeats. Wire saturation is already pinned at
`test_emission_secs_saturates_at_wire_field_max`
(`transport.rs:19197`).

**5. Emitted high-water** ([healing loop](#the-healing-loop)).
Unit: no re-anchor — including an arms-1–2-quality backwards
correction — takes the calendar below the highest value this
identity has emitted; the correction floors at the high-water mark
(the below-floor attempt is the negative cell). Integration:
persistence of the mark across a restart where storage exists; the
RTC-less mitigation is pinned at
`test_own_announce_echo_reseeds_timebase_before_echo_drop`
(`transport.rs:18765`). The announce-timebase consequence — a peer
keeps the newest path — is shown against a Python-shaped peer:
conformance `time_high_water_path_retention`, where a re-anchored
`lnsd` keeps its announces ordered above its own high-water and
the Python peer's path entry keeps updating.

**6. Healing loop** ([healing loop](#the-healing-loop)). mvr, one
cell per branch of the re-anchor rule: a median over a cohort of
distinct senders re-anchors a grossly-forward calendar; a single
sender does NOT re-anchor a calendar past rank 5 (negative; the
rank-5 single-source adoption is rule 3's cell); a gross backwards
correction via traffic alone is refused, including a fabricated
traffic median (negative). The Sybil-style bound —
N identities from one neighbour advance the calendar no further
than the documented N × cap — is pinned at
`test_timebase_walk_is_capped_per_announce_not_per_identity`
(`transport.rs:19039`) and stays a negative cell: the assertion is
the bound, not a defence the model does not claim. RTC write-back
on re-anchor is a unit cell against a mock RTC; the real
peripheral is rule 10.

**7. Echo chamber** ([the one honest cost](#the-one-honest-cost)).
Conformance `time_echo_chamber_third_peer`: two cold, build-floor
nodes adopt each other's birth clocks without harm — traffic
flows, nothing poisons — and BOTH converge once a third peer with
real time appears. The provenance transitions (birth → anchored
from traffic, unconfirmed → healed) are observable in the
structured event log of both nodes; a run that converges without
showing the intermediate state fails the cell.

**8. Ingress clamp + collector**
([Ingress](#ingress-clamp-for-semantics-keep-for-display),
[Telemetry](telemetry.md), Codeberg #239). Unit: a future-stamped
row is clamped for cursor and index and served with the clamped
value; dedup still catches a twice-delivered row despite clamping
(dedup keys are not clamped); an UNHEALED collector does not clamp
(the arming rule's negative). Conformance
`time_collector_clamp_cursor`: the cursor of a synced requester is
never poisoned by a future-stamped row — the cell reproduces the
pre-fix starvation shape and asserts it gone. Conformance
`time_collector_unhealed_heals`: an unhealed collector takes
in-window stamps as-is and heals from the very traffic it is
collecting.

**9. End-to-end authorship interop.** Conformance against real
Python, drop-in discipline both ways.
`time_cold_authorship_python_receiver`: a cold-start `lnsd`
authors LXMF to a Python receiver; the message is accepted and
readable — it sorts old, so the cell asserts delivery, never sort
order — and after healing, stamps are current. The reverse cell,
`time_python_future_stamps`: Python sends future-stamped traffic
at our stack; nothing breaks and nothing poisons — cursors are
asserted not to advance past local plausible-now, and path tables
stay intact.

**10. Hardware tier** — only where the subject is real.
`time_gnss_seed_rig`: GNSS time seeding on the rig boards, lands
with the GNSS issues (#69/#70). `time_rtc_writeback_rig`: RTC
write-back on a board with an RTC, lands with the write-back
implementation. Both are marked "lands with the implementing
issue", not pre-existing debt: the rows exist precisely so those
issues cannot land without their cells.

### Meta-rules

- **Every implementing issue carries its cells from this matrix.**
  The issue is not done until its cells are green in the tier
  named here, in the same change.
- **A rule without a named cell is a spec hole.** The fix is a new
  cell in this matrix — added to it, and to the port checklist
  where the rule touches a port duty — before the rule is
  implemented, not after.
- **Negative cells are mandatory wherever a rule refuses
  something.** A refusal without a red-path test is a refusal
  nobody has seen fire; every "refused", "never" and "does not" in
  this page has its negative cell above.

## Checklist for a new firmware port

1. **Inventory the arms.** Which of the five can this platform
   offer? (GNSS receiver? an attached host? OS/RTC clock?)
2. **Implement `Clock::wall_unix_secs` only if the platform has a
   real wall clock.** Returning `None` is correct and engages the
   ranking. Never return an uptime-derived value from it.
3. **Plumb the build timestamp — and the rank switch with it.** The
   build timestamp is the sanity floor and the birth anchor; a port
   without it is still living in the raw-uptime state the model
   retires. The same change must move the adoption and ticket
   predicates from value tests to the anchor rank
   ([provenance rank](#anchor-provenance-is-first-class-state)), or
   it regresses cold-start healing and ticket refusal in one step.
4. **Wire every available better source.** GNSS: seed from RMC UTC
   through the sanity window. Attached host: implement the
   control-channel frame that calls `set_wall_time_unix_secs`.
5. **Write healed anchors back to the RTC**, where one exists.
6. **Do not touch announce learning.** It comes with the core for
   free. Do not disable it, and do not "improve" it with local
   filtering of incoming timestamps — that is a semantic deviation
   from the reference (see the non-behaviours above).
7. **Never stamp cross-lifetime wire fields from `now_ms`.** New
   fields go through `Transport::emission_secs`.
8. **Expose the time source** for diagnosis (see
   [Record the source](#record-the-source)).
9. **Leave the pins green.** The tests cited throughout this
   document are the contract; a correct port never needs to change
   them. The full rule-by-tier map is
   [Testing the model](#testing-the-model) — a port's own issues
   land with their cells from it.
