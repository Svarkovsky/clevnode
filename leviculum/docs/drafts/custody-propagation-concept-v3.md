# Custody-Based Propagation & Multi-Device for LXMF/Leviculum — Concept v3.1 (WIP)

**Status:** Work in progress — draft, not final. Supersedes v2 entirely. Folds in the scoped
feasibility re-check of the v2 §9 items (leviculum `01617ebf`, LXMF 1.1.0,
reticulum-kt v0.0.22, Columba master). All previously open decisions are
resolved here; the changelog against v2 is §10. This document is the direct
input for **spec v1** (§8 defines what spec v1 must contain).

---

## 1. Design invariant

Custody of an unacknowledged message never leaves the sender. The network
between the endpoints is an ephemeral, best-effort cache fabric with no
delivery obligations. Durable copies exist in exactly two places: the sender's
outbox (until acknowledged or given up) and the recipient's inbox.

**Two custody levels (normative distinction):**

- **Sender custody** — the outbox entry. Never transfers, never evicts
  (§2.4). This is the invariant.
- **Cache tokens** — copies held by intermediate nodes. Not custody:
  transferable, evictable, losable. Loss of any token is recoverable via
  sender re-injection.

Non-delivery is not prevented (impossible over an unreliable medium) but made
detectable: an unacknowledged outbox entry is visible state, not silent loss.

Not touched: RNS core routing (announces, links, proofs, paths suffice —
the only core additions are two small accessors, §7.3), the LXMF envelope,
direct link delivery.

## 2. Component A — Sender outbox with end-to-end receipts

### 2.1 Outbox state machine (leviculum-lxmf)

- Receipt drives the missing transition `AwaitingCollection → Delivered`.
- Retry: exponential backoff, no hard give-up. `Failed` reserved for genuine
  refusals; new terminal `GivenUp` entered only on sender-TTL expiry or
  explicit user discard. Snapshot v5.
- Re-injection triggers: backoff timer AND `RouterEvent::PeerAnnounced` for
  the recipient's destination.
- Receipt emission hook `accept_inbound`; on duplicate arrival re-emit the
  receipt (the duplicate IS the receipt-retransmission request). Receipts are
  stateless, never custody-tracked — no recursion. API: widen
  `RouterEvent::Duplicate` to carry the source hash, or add
  `ReceiptDue { source_hash, message_id }` (public API break, three
  consumers, do it once).

### 2.2 Receipt format

Standalone LXMF message, empty title/content, using the extension map (§7.1):

```
fields[0xFC] = { "lxmf.receipt.v1": [ <id0>, ... ] }   # 16-byte truncated message IDs
```

- Cumulative, **up to 14 IDs per opportunistic packet** (payload 37 + 18k,
  `packed[16..]` 117 + 18k against `ENCRYPTED_MDU = 383`; the Python
  admission constant 295 sits 8 bytes above the real MDU — stay at 14, clear
  of the upstream off-by-one).
- Bloom filters rejected: a false positive marks an undelivered message
  delivered — the exact silent loss this design removes.
- Piggyback-on-reply is the preferred carrier when a reply is going out
  anyway (the extension map makes receipt + other extensions on one message
  possible); standalone otherwise. Short receipt TTL.
- **Capability-gated, mandatory:** receipts only to peers advertising
  `SF_CUSTODY` (§7.2). Reason: Sideband renders unknown field-bearing
  messages as visible empty messages.
- Stamp cost: receipts pay inbound stamp cost like any message; use tickets
  (`FIELD_TICKET`, existing) issued in normal conversation, mine PoW only
  without one.

### 2.3 Client side (Columba)

Above the backend seam (Room DB layer), backend-agnostic: pending-delivery
scan job, scheduler, receipt compose/parse via `MessageOptions.extraFields`,
delivery-status UI (in transit / delivered / undelivered after N attempts /
given up), and surfacing of `QueueFull` (§2.4).

### 2.4 Outbox sizing and eviction (normative)

- **The outbox never evicts.** `RouterError::QueueFull` at capacity is a
  modal, user-visible refusal at send time — never a background drop.
  The only exits from the outbox are acknowledgement and explicit/TTL
  give-up (user can retry or discard from `GivenUp`).
- **Eviction rewrite before raising bounds:** `insert_bounded_id` is an O(n)
  scan per inbound message at capacity; add a secondary insertion-order /
  timestamp index (O(log n) or O(1)) first (~150 LoC), then raise.
- `delivered_ids` eviction is invisible-but-harmful (evicted ID = duplicate
  chat bubble); expose a host-readable eviction counter.
- **Three host profiles** as named `RouterConfig` constructors:

| | constrained (LNode) | mobile (Columba) | host (lnsd/desktop) |
|---|---|---|---|
| `max_outbound` | 32 | 2 048 | 16 384 |
| `max_delivered_ids` | 512 | 32 768 | 262 144 |
| `max_processed_ids` | 512 | 8 192 | 65 536 |
| `max_snapshot_bytes` | 64 KiB | 64 MiB | 512 MiB |

## 3. Component B — Push-on-announce (cache behavior)

- Cache listens to `lxmf.delivery` announces, matches its store by the
  cleartext destination-hash prefix of stored blobs, pushes immediately.
- **Deletion rule 1 (delivery push): only on proof, never on transmission.**
  Proof sources: the recipient's automatic `packet.prove()` on the
  opportunistic path (returns to the cache), or an observed custody receipt.
- Legacy clients: **pull-only in phase 1** through the facade. (The narrow
  ≤295-byte opportunistic push path via a new
  `send_single_packet_preencrypted` core entry point remains an option,
  deferred until measurements justify the API surface.) Between
  custody-capable nodes, push is a new inbound path and carries any size.
- Contention control at the interface layer (interface-isolation rule):
  randomized pre-push delay weighted by announce recency / hop distance,
  abort on detected concurrent transfer. No propagation-aware state in the
  node runtime.

## 4. Component C — Budget caches + legacy facade

- Any node caches under purely local policy: byte budget, hard TTL, age
  eviction. Eviction is normal operation; the sender outbox is the backstop.
- **TTL rule (spec v1):** cache expiry = signed LXMF timestamp
  (`payload[0]`) + network constant (order 48 h). No wire change,
  enforceable without trusting the holder. Sender-set TTL deferred; if ever
  added it lives inside the signed payload. Clockless nodes cannot enforce
  this — one reason LNode caching is a later phase (the other: no QSPI flash
  driver; an LNode cache today is RAM-only, ~10 small messages).
- Storage: `LxmfStorage` seam as-is; cache entries keyed
  `cache/<dest_hash>/<transient_id>` with a per-message record
  (`copies_held`, `last_forwarded_ms`, per-peer offered set) — supports both
  announce-triggered lookup and focus candidate scan.
- **Legacy PN facade (kept, ~700–1 000 LoC):** valid PN announce app data,
  packet + resource upload paths with stamp validation, `/get` with
  list/haves/wants + transfer limit, identity→delivery destination
  derivation. `/offer` answered `ERROR_NO_ACCESS`; control destination
  omitted. The facade advertises its actual retention via
  `PN_META_CUSTOM (0xFF)`.
- No mailbox semantics: long-offline recipients receive whatever sender
  outboxes still re-inject; message lifetime is the sender's patience, not a
  stranger's storage pressure.

## 5. Component D — Spray/Focus protocol (spec v2, own destination)

- **Own destination aspect** (`lxmf.custody`), envelope
  `[timebase, [stamped_lxmf], copies, ttl, ...]`, spoken only between nodes
  advertising `SF_CUSTODY`. The copy counter can live neither inside the
  signed envelope nor appended to the legacy upload array (silently dropped
  on the Python resource path).
- **Summary-vector handshake before any copy transfer** (tokens are never
  spent on peers that already hold the message), raw fixed-width encoding:

  ```
  u8 type/version | u8 flags (bit0 = continuation) | u8 count | [k * 8] ids
  ```

  **8-byte truncated transient IDs, frozen.** 21 IDs per single LoRa frame
  (176 plaintext bytes/frame); accidental collision space 2^64; adversarial
  single-suppression cost 2^64 hashes — documented and accepted. Chunkable
  via continuation flag; the runtime queries the frame budget through the
  new `NodeCore::egress_mtu(&DestinationHash)` accessor (§7.3) — never
  hardcodes it. The handshake also carries the **transfer acknowledgement**
  for focus handovers (deletion rule 2).
- Binary spray: holder with n > 1 tokens transfers ⌊n/2⌋ to an empty peer
  (post-handshake).
- **Focus phase, mandatory:** with n = 1, forward to a peer with strictly
  better utility by at least threshold `U_th` (measured parameter).
  **Deletion rule 2 (focus handover): the forwarder deletes on transfer
  acknowledgement from the receiving cache.** The ack is a single frame and
  need not be reliable — a lost token after ack is recovered by sender
  re-injection. Rules 1 and 2 are distinct and both normative; conflating
  them either floods (keep) or silently loses tokens without a handshake
  (delete).
- **Utility = locally observed announce recency of the recipient's
  destination. Nothing else.** No encounter-transitivity, no utility-value
  exchange in phase 1: locally-observed recency cannot be forged; any
  exchange mechanism makes the gradient attackable. Periculum may later
  quantify what transitivity would buy in announce shadows.
- **Copy budget:** `L_effective = min(f(stamp_value), L_max_local, g(M̂))` —
  stamp-bound (only inflation anchor already on the wire, locally
  recomputable), hard local ceiling, scaled with M̂.
  **M̂ = count of distinct `lxmf.delivery` destinations seen within the
  announce window** (the filter already runs; zero new bookkeeping). This
  deliberately measures the announce horizon, which is the encounterable
  population — the right M for spray. Rule of thumb from the literature:
  L ≈ 5–10 % of M̂ for spray+focus; Periculum decides.
- Residual risks (accepted, documented): black-hole holders degrade affected
  traffic to outbox-retry-only; destination-hash-level interest leakage is
  inherent to gradient designs; replay bounded by the signed-timestamp TTL.

## 6. Multi-device

### 6.0 Status quo, normative

Copying an identity key to two devices is **unsupported and documented as
broken**: the path table holds one entry per destination hash (routing
degrades to last-writer-wins) and a sender encrypts opportunistic traffic to
the single last-cached ratchet — one device cannot decrypt. Code-confirmed.

### 6.1 Stage 1 — Master/mirror with lease (app-level, no core change)

- Both devices store the user identity; only the **master** uses it on the
  network. The mirror replicates over a private device-sync channel and is
  UI-equivalent.
- **Ratchet model (corrected):** ratchets are independently generated keys;
  there is **no chain and no derivation**. What the mirror must hold is the
  **retained set** of own-ratchet private keys, so it can decrypt anything a
  sender encrypted against any previously announced ratchet. After
  promotion it generates and announces fresh ratchets normally.
  Implementation facts (verified): `serialize_ratchets_signed` /
  `load_ratchets_signed` exist, are public, Python-compatible and
  self-authenticating (inner list signed by the owning identity); retention
  is count-based (512), so the timestamp loss on import is harmless; the
  promotion shape is already a passing core test. Sync payload:
  `{ destination_hash, ratchets: <signed blob verbatim> }` — the blob
  **replaces, never merges** (merging divergent sets truncates
  unpredictably), and must travel confidentially (private keys). The
  peer-ratchet half is optional; its loss costs only efficiency.
- **Promotion sequence (Columba):** write ratchet blob into the backend's
  ratchet store → write/activate identity → **restart the LXMF router** →
  announce. Columba's canonical backend is Torlando's Python-RNS fork
  (tracking upstream + patches); the restart requirement was verified on
  reticulum-kt (ratchets load only at destination construction; live
  injection is a silent no-op) and Python RNS reloads at construction the
  same way — but **verify against the canonical backend before spec v1
  §8.6 freezes the sequence**.
- **Mastership is a lease** (record: `master_device_id, lease_expires_at,
  epoch`), renewed on every device sync:
  1. the mirror promotes only after lease expiry + safety margin;
  2. the master **self-demotes** the moment it fails to renew — stops
     announcing, sending, receiving.
  Double-mastership is bounded by clock skew, not partition length.
  Failure asymmetry: a masterless interval costs latency, never loss (all
  undelivered traffic sits in sender outboxes) — conservative margins are
  free. Manual override in UI ("make this device master now", warning while
  a lease is live).
- **Device identities:** devices talk under their own RNS identities
  (the user identity is exclusively the master's). In Columba these live in
  their **own table**, not in `local_identities` — the "one active
  identity" invariant keeps its single meaning, and device identities own
  no conversation partition.
- **Sync channel: over RNS, gated.** Before any bulk sync:
  `getHopCount == 1` AND next-hop interface is not LoRa
  (`getNextHopInterfaceName`); over LoRa paths only the lease renewal (one
  frame) runs, never store replication. Transport is app-framed chunking
  over link packets (~600–900 LoC resumable transfer) **or** Resource
  exposed through the backend seam — implementer's choice, Resource
  preferred if the seam change is acceptable.
- **Prerequisite (also a standing Columba hazard):** on the canonical
  Python-fork backend the migration exporter works (the ratchet directories
  it reads exist). The hazard is the abandoned-but-still-selectable
  Kotlin-native backend: its `FileMigrator` deletes both ratchet
  directories on every start — including files freshly written by the
  Python backend — making history permanently undecryptable after one
  accidental backend switch. **Defuse `deleteLegacySourceFiles()`**
  (~50 LoC) while the Kotlin path remains in the binary, or remove the
  path. Report/land independently of this project; precedes stage 1.
- Stage 1 is the permanent fallback toward legacy contacts.

### 6.2 Stage 2 — Attested device list + fan-out (between capable clients)

- Each device gets its own network identity/destination. One-time pairing
  ritual (QR): the devices **mutually sign** — A signs "B is my device", B
  signs "I belong to A" — and each stores the counterpart signature.
- **Wire format (decided): compact-unilateral.**

  ```
  fields[0xFC] = { "lxmf.devices.v1": [version, counter, [h0..hk], owner_sig] }
  ```

  191 + 18k bytes total against the 383-byte opportunistic ceiling: fits
  with content room up to ~10 devices. The **mutual form does not fit at
  k ≥ 3** (191 + 84k) and is therefore not the wire default; it is verified
  at pairing time and **available on request over a link** for contacts that
  want device-consent proof. Residual risk of the compact form (owner lists
  a destination that is not theirs): fan-out is additive, equivalent to
  voluntary forwarding by the owner — accepted, documented.
- **Versioned (monotonic counter, latest signed version wins) with
  first-class revocation:** stolen device → signed revocation, effective
  immediately at every client that sees it; stale-list replay defeated by
  the counter.
- **Attach policy:** never on every message. On first contact, on version
  change, on request.
- Capable senders fan out: one outbox entry **per recipient device**, each
  copy individually encrypted/delivered/receipted. Sender-side "delivered"
  = first receipt from any attested destination; remaining entries continue
  as background custody until receipt or sender-TTL. Caches delete on
  proof/receipt, full stop — per-device copies make grace windows
  unnecessary (and in stage 1 only the master touches the network).
- Byproducts: BCC semantics (per-recipient encryption) and sender-local
  distribution lists (client UI). **Out of scope:** real groups with
  join/leave — that is MLS work (group key agreement, epochs, cryptographic
  removal), a separate track; the attestation asserts "one person, one
  trust domain" and must not be abused for groups.

## 7. Registry (single source of truth, finalized)

### 7.1 Extension map

`FIELD_CUSTOM_DATA (0xFC)` is a **map keyed by type string**; `0xFB` is not
used. Multiple extensions coexist on one message (receipt + attestation on
the same reply is the design case):

```
fields[0xFC] = {
  "lxmf.receipt.v1": [ <16B id>, ... ],          # §2.2
  "lxmf.devices.v1": [version, counter, [...], owner_sig],  # §6.2
}
```

Verified conflict-free: Sideband, NomadNet, Columba, MeshChatX use nothing
at `0xFB`/`0xFC` (historical squats are at `0x10` and `0x70`/`0xFD`).

### 7.2 Capability advertisement

Feature codes in the **existing features list** (element 2 of the
`lxmf.delivery` announce app data) — not positional elements:

| Code | Name | Meaning |
|---|---|---|
| `0x00` | `SF_COMPRESSION` | (existing, upstream) |
| `0x01` | `SF_CUSTODY` | understands receipts, custody push, `lxmf.custody` |
| `0x02` | `SF_DEVICES` | understands `lxmf.devices.v1` attestations |

Trailing-element and unknown-code tolerance verified on all three stacks
(Python guards `len < N`; ours skips; LXMF-kt never reads index 2).
**Normative:** feature codes are unsigned integers, nothing else — a
non-integer entry hard-fails the whole announce decode on leviculum peers.
A future capability that needs a *value* gets one positional element holding
a map, allocated then, not now.

### 7.3 Core additions (complete list)

| Addition | Size | Consumer |
|---|---|---|
| `NodeCore::egress_mtu(&DestinationHash) -> Option<usize>` (via path table `interface_index`) | ~40 | summary vector §5 |
| Known-destination / identity enumerator on `Storage` (optional; only if the delivery-announce M̂ proxy proves insufficient) | ~30–60 | M̂ §5 |
| `send_single_packet_preencrypted` (deferred, phase-2 option) | ~30 | legacy small-push §3 |

Nothing else touches `leviculum-core`. Ratchet export needs zero core work.

### 7.4 Other allocations

| Item | Home |
|---|---|
| Facade retention | `PN_META_CUSTOM (0xFF)` |
| Spray envelope | own destination aspect `lxmf.custody`, own encoding — never an LXMF field |
| Summary vector / transfer ack | custody-aspect wire protocol, §5 encoding |

## 8. Spec v1 contents (checklist)

Spec v1 is short, standalone, and written before code so microReticulum and
Columba can implement independently:

1. Receipt format (§2.2) incl. the 14-ID packet bound and ticket rule.
2. Outbox semantics: states incl. `GivenUp`, retry policy, no-eviction rule,
   duplicate→re-receipt rule, receipts-never-custody-tracked.
3. Registry: `0xFC` map, `SF_CUSTODY`/`SF_DEVICES`, integer-only feature
   codes (§7).
4. Cache TTL rule (timestamp + network constant) and deletion rule 1.
5. Facade retention advertisement.
6. Stage-1 device sync: ratchet-set model (explicitly: no chain, replace
   never merge, confidential transport), promotion sequence, lease rules.

Spec v2 (later, after Periculum): custody aspect envelope, summary vector +
transfer ack, deletion rule 2, spray/focus rules, `U_th`, `L_effective`
formula with measured parameters, attestation request-over-link flow.

## 9. Effort and sequencing (consolidated)

| Work | LoC est. | Depends on |
|---|---|---|
| Spec v1 (§8) | doc | — |
| A in leviculum-lxmf (incl. dedup-eviction rewrite, host profiles) | 650–1 000 | spec v1 |
| A in Columba (scan job, receipts, status UI) | 1 500–2 500 | spec v1 |
| **Columba: defuse legacy `FileMigrator` ratchet deletion** | ~50 | — (do first, independent value) |
| Periculum: propagation verbs + delivery log | 500–800 | — |
| C: cache store + policy | 400–700 | — |
| Legacy PN facade | 700–1 000 | C |
| B: announce matcher, push scheduler, proof accounting | 900–1 400 | C |
| Core: `egress_mtu` (+ optional enumerator) | ~40–100 | — |
| Periculum: emulator control socket + contact schedules | 400–700 | — |
| Periculum: mobility trace generator (home zones, p_l/p_r, ~10 % static) | 150–300 | control socket |
| Spec v2 | doc | measurements |
| D: spray/focus incl. summary-vector codec + handshake | 1 450–2 250 | spec v2, emulator |
| Multi-device stage 1 (Columba: transport ~900, replication ~700, lease+promotion ~500, UI ~400) | 2 500–4 000 | FileMigrator defusal, spec v1 §8.6 |
| Attestation codec + pairing (stage 2) | ~400 | registry |

**Critical path:** registry decisions (now frozen in §7) → spec v1 → A.
C/facade/B parallel to A. Periculum control socket is the long pole for D —
do not start D before it exists. Multi-device stage 1 can start once spec v1
§8.6 is written; it needs no core change.

**Separate track, not a dependency:** a Leviculum-based Columba backend
(leviculum-core + leviculum-lxmf behind the existing `RnsCore` seam, shipped
as a third opt-in flavor). It would collapse most of the Columba-A effort
(the outbox machinery lives in `leviculum-lxmf`), but nothing in this
roadmap waits for it: Component A stays app-layer over the canonical
Python-fork backend. Demo gate: after A+C exist — feature demo (delivery
status, offline compose, un-send) plus cold-start / process-kill-recovery /
RSS measurements with pre-registered methodology; no battery claims.

**Acceptance milestone (phase 1):** two clients + one cache node on a
three-container topology. Alice→Bob while Bob is partitioned; cache accepts;
Bob reconnects and announces; cache pushes; Bob receipts; Alice's outbox goes
`AwaitingCollection → Delivered`; cache deletes only after proof. Kill and
restart Alice mid-flight: the outbox entry survives and the receipt still
lands.

## 9a. Reviewer-Anmerkungen (Review 2026-09-01, nicht normativ)

Drei kleine Punkte für Spec v1, aus dem Erst-Review; keiner stellt eine
Entscheidung dieses Dokuments in Frage:

1. **QueueFull auf kopflosen LNodes (§2.4):** „modale, sichtbare
   Verweigerung" hat auf einem Board ohne UI keine Bedeutung. Spec v1
   sollte definieren, wie sich `RouterError::QueueFull` bei
   `max_outbound = 32` auf einem LNode äußert (Fehler an den seriellen
   Aufrufer / Statusanzeige), bevor das Profil normativ wird.
2. **Re-Injection-Herde (§2.1):** `RouterEvent::PeerAnnounced` als
   Trigger lässt nach dem Wieder-Announce eines lange offline gewesenen
   Empfängers viele Sender-Outboxen gleichzeitig feuern. Komponente B
   hat Contention-Control, Komponente A nicht — ein randomisierter
   Jitter vor der Re-Injection wäre ein Ein-Satz-Zusatz in Spec v1.
3. **Receipts als Metadaten-Leck (§2.2/§5):** Cache-Knoten, die
   Custody-Receipts beobachten (Löschregel 1 setzt das voraus), lernen
   Zustellmuster. Gehört in die dokumentierte Residual-Risiko-Liste
   neben das Interest-Leakage — akzeptierbar, aber benennen.

## 10. Changelog v2 → v3

1. Ratchet "chain" model replaced by **retained-set** model (no derivation
   exists; inventing one would break Python compatibility). Promotion
   sequence and blob semantics (replace-never-merge, confidential) made
   normative.
2. §3/§5 contradiction resolved via **two deletion rules** on the two
   custody levels: delivery push deletes on proof; focus handover deletes on
   transfer ack (carried by the summary-vector handshake).
3. Registry rebuilt: `0xFC` as type-keyed **map** (receipt + attestation can
   share a message), `0xFB` dropped; capabilities as **feature codes**
   `SF_CUSTODY`/`SF_DEVICES` in the existing features list, not positional
   elements; integer-only rule added.
4. Receipt packet bound corrected to **14 IDs** (was 9); upstream
   off-by-`TIMESTAMP_SIZE` noted, stay clear of the edge.
5. Attestation wire format decided: **compact-unilateral** on the wire,
   mutual signatures verified at pairing and available on request over link;
   attach policy (first contact / version change / request) added.
6. Summary vector frozen: 8-byte IDs, raw fixed-width, continuation flag,
   budget queried via new `egress_mtu` accessor.
7. M̂ decided: distinct `lxmf.delivery` destinations in the announce window
   (zero new bookkeeping); explicitly the encounterable population.
8. Outbox: **never evicts** (QueueFull is a surfaced refusal); dedup
   eviction rewrite mandated before raising bounds; three host profiles
   added.
9. Device identities: own table in Columba; sync channel over RNS gated on
   hop count + interface type (no bulk sync over LoRa).
10. Columba migration-exporter repair added as an independent prerequisite
    (standing data-loss bug: `FileMigrator` deletes the directories the
    exporter reads).
11. Legacy small-message push (`send_single_packet_preencrypted`) demoted to
    a deferred phase-2 option; phase 1 legacy delivery is pull-only via the
    facade.

### v3 → v3.1

12. Columba's canonical backend is Torlando's Python-RNS fork; the
    Kotlin-native backend is abandoned. Promotion sequence (§6.1) must be
    verified against the canonical backend before spec v1 §8.6 freezes it.
13. Exporter-repair prerequisite reframed as **FileMigrator defusal**
    (§6.1, §9): the exporter works on the Python backend; the hazard is the
    still-selectable Kotlin path deleting ratchet directories — including
    Python-written ones — on any start.
14. Leviculum-as-Columba-backend added as an explicit **separate track**
    (§9) with its demo gate; no roadmap item depends on it.
