# Why a 24-symbol preamble breaks the mixed pair — source + log analysis

Batch deliverable, 2026-08-21. No hardware run; sources plus the two
existing full-corpus runs of the day (~02:00 and ~14:00, both RED on
`lora_path_discovery_slow_mixed_preamble24`, everything else in the
family GREEN).

## Verdict up front

Both ends handle the number 24 correctly and identically at the source
level. There is no units confusion, no cap, no detector-bits mixup
anywhere in the configuration path. The break is below anything our
sources (or the RNode firmware's) control: the SX1276 fails to receive
frames whose preamble is 24 symbols (~197 ms) at SF10/BW125, while it
receives the same frames at 18 symbols (~148 ms), and while the SX1262
receives in the opposite direction under the same mismatch without
loss. The suspicion the instruction anticipated — "then it moves to
timing" — lands specifically on the SX1276 demodulator's
preamble-to-sync tracking, not on any timeout in our stacks.

## 1. The configuration path is clean (instruction items 1 and 3)

`preamble_symbols = 24` travels as **symbols** end to end:

- periculum `[radio] preamble_symbols` → rendered into the receiver's
  `[[Serial LNode]]` block; only the T114 takes the key (the RNode
  firmware derives its own preamble and exposes no host key).
- lnsd: a pinned value overrides the derivation —
  `leviculum-std/src/interfaces/serial.rs:95-97`
  (`cfg.preamble_symbols.unwrap_or_else(|| derive_preamble_symbols(..))`).
- Wire frame: `freq(4) bw(4) sf(1) cr(1) txp(1) preamble(2 BE)` —
  `leviculum-core/src/rnode.rs:1038`.
- Firmware: `RadioConfig::from_wire_config` passes it through untouched
  (`leviculum-nrf/src/lora.rs:232`), `configure_lora` stores it
  (`leviculum-nrf/src/sx1262.rs:447`), and the **only** hardware
  consumer is `SetPacketParams` bytes 0–1
  (`leviculum-nrf/src/sx1262.rs:417-418`). The SX1262 interprets that
  field as a plain 16-bit symbol count; on-air preamble = the
  programmed count.
- Applied, not just sent: the red run's receiver debug UART shows
  `radio config received` followed by
  `active config: freq=869525000 sf=10 bw=125000 cr=8` with the 24 in
  force (the subsequent TX airtimes match: `op=tx duration_ms=2727` for
  a 184 B frame = 24-symbol preamble + payload at SF10/CR8, vs 2530 ms
  for the 168 B announce).

The known trap `LORA_PREAMBLE_TARGET_MS = 24`
(`leviculum-core/src/rnode.rs:780`) is a **coincidence of numerals
only**: that constant feeds `derive_preamble_symbols`
(`rnode.rs:839-856`), which the pinned key bypasses entirely
(`serial.rs:95-97`). Nothing on this path divides by symbol time or
otherwise reinterprets 24 as milliseconds.

## 2. The RNode side never sees the number (instruction item 2)

The RNode derives its own preamble: 18 symbols at SF10/BW125
(`RNode_Firmware/Config.h:84-87`, `Utilities.h:1245-1258` — 24 ms
target / 8.192 ms per symbol = 2.9, floored to the 18 minimum). TX
programming subtracts 4 because the SX127x hardware appends 4.25
symbols (`sx127x.cpp:450-454`). On the RX side there is **nothing** to
mis-handle: reception runs in `MODE_RX_CONTINUOUS`
(`sx127x.cpp:329-335`), which takes no preamble-length parameter, has
no symbol timeout in play, and no clamp; `sx127x::dcd()` is a stateless
modem-status read (`sx127x.cpp:197-203`); the detection registers are
the standard SF7-12 values (`sx127x.cpp:381-396`). The
preamble-time-dependent false-preamble logic exists only in the SX126x
driver (`sx126x.cpp:495-521`) and only gates CSMA — and the sender here
is an SX1276.

## 3. The detector-bits hypothesis is refuted (instruction item 4)

The 8/16/24/32-**bit** preamble detector lengths belong to the SX126x
**FSK** packet engine's `SetPacketParams` variant. The LoRa variant has
a plain 16-bit symbol count and no detector-length parameter at all.
Our driver is LoRa-only and never issues `SetLoRaSymbNumTimeout`
(default 0 = validate on first detected symbol). Neither end programs
24 as detector bits, and neither derives the RX detector from the ms
target.

## 4. What the existing logs add: the failing arc is LNode→RNode only

Both 2026-08-21 red runs, from the saved artifacts (receiver debug
UART + merged daemon logs), no new hardware:

- **LNode TX → RNode RX: total loss.** Run 2: the T114 keyed 7 frames
  after the preamble-24 config (1× its 168 B announce, 6× the 184 B
  transport announce / path response), each with `TX done` and
  airtime-correct duration. The sender daemon logged **zero** `PKT_RX`
  on `rnode_0` for the whole scenario. Run 1: same shape (9 frames
  keyed, nothing received). The grep pattern was positively controlled
  on the green sibling, where it fires 3 times — twice on the
  *identical 184 B frame type* that went unreceived in the red cell,
  and once on the identical 168 B announce.
- **RNode TX → LNode RX under the same mismatch: clean.** All three
  path requests (52 B, preamble 18 from the RNode) were received by
  the T114 at SNR 8–9 in run 2 (`T114_SX_RX len=52`), three in run 1
  as well.

This **refutes the direction recorded in
`periculum/hardware/lora_path_discovery_slow_mixed_preamble18.toml:48-55`**
("what fails in this corner is RNode → LNode reception") — that note
described the pre-continuous-RX build and does not hold today.

Duration vs symbol count, from the corpus:

| cell | preambles on air | duration | verdict |
|---|---|---|---|
| fast_mixed (SF7/CR5) | 24 / 24 sym | 24.6 ms | GREEN |
| slow_mixed + preamble18 (SF10) | 18 (both arcs) | 147.5 ms | GREEN |
| slow_mixed_preamble24 (SF10) | LNode 24 / RNode 18 | 196.6 ms | RED, 0 % on the LNode→RNode arc |
| lnode_pair at 24/24 (historical) | 24 / 24 sym | 196.6 ms | 18/20 — SX1262 RX copes |

So: 24 *symbols* are fine (SF7), ~197 ms is fine into an SX1262 RX,
~197 ms into the **SX1276** RX is a hard fail, ~148 ms into the same
SX1276 is fine. The threshold, whatever it is, sits between 148 and
197 ms at BW125 and lives in the SX1276 demodulator (post-preamble-
detect sync-word search window / preamble tracking), which no source we
control configures. The historical 4/20–5/20 (rather than 0/20) fits a
phase effect: a receiver that starts its preamble search mid-preamble
(coming out of its own TX or CAD) sees less than the window and
decodes — deterministic mechanism, phase-dependent symptom. A second,
less likely PHY suspect is the measured ~10 kHz T114 carrier offset at
SF10 interacting with the longer tracking time; the experiment below
distinguishes both from a stack-level cause without further analysis.

## 5. The one distinguishing rig experiment

**Preamble ladder on the failing arc, raw-KISS receiver.**

- T-Beam-1 as receiver in raw KISS capture (no daemon — the #313
  method, so our stack is out of the RX path entirely).
- T114 via lnsd transmits N=20 identical ~184 B frames per rung.
- PHY fixed: SF10 / BW125 / CR4:8 / 869.525 MHz / 2 dBm, quiet channel.
- One variable: `preamble_symbols` ∈ {18, 20, 22, 24, 28}
  (147 / 164 / 180 / 197 / 229 ms).

Expected outcomes, fixed in advance:

- **Cliff** (some rung ~20/20, next rung ~0/20): confirms the SX1276
  RX preamble-duration ceiling; the cliff rung × 8.192 ms names it.
  Optional same-session cross-check: SF9 with 48 symbols (~197 ms) —
  red there means duration-based, green means SF10-specific.
- **Flat 20/20 through 28**: refutes the PHY hypothesis entirely; the
  scenario red then comes from timing interplay above the radio, and
  the next hypothesis has to be built from a merged two-board timeline
  of the full scenario, not from this analysis.

Either outcome decides the fix direction. Note the compatibility
angle if the cliff is real: the RNode firmware derives its preamble and
can never key >24 ms itself, but any third-party SX126x stack (or a
future config default of ours) that keys long preambles at slow SF
would show exactly this one-way loss toward SX127x peers.

## 6. Side observation (separate from this bug)

In serial-LNode mode the T114's **onboard node stack is live alongside
the modem role**: it consumed all three path requests itself
(`T114_LORA_DELIVER`, never forwarded up the serial — the daemon logged
no `PKT_RX` for them) and answered from its own announce cache, and it
rebroadcast the daemon's announces back up the serial (the daemon's
own announce returned as hops=2, +16 B transport header, 359 ms after
sending — timing only a serial loop can produce). In the green cell
this double-transport arrangement happens to work (the sender resolved
the path via the board's transport announce), and it is not the red
cause here, but two transport instances share one radio and one of
them is invisible to the harness. Worth a deliberate decision.
