# lnmsg: a terminal LXMF messenger

**Status: design record. All ten open questions are decided.**

This began as a draft for argument in which nothing was decided. Every
significant decision was presented as options with trade-offs and a stated
preference, and the preference was an opening position rather than a
conclusion. Between 2026-08-08 and 2026-08-10 all ten questions were
settled, so the document graduates from discussion to record and the
remaining work is issues and batches.

The rejected alternatives are kept throughout, on all five pages. They are
the *why*: a decision recorded without the options it beat is an assertion,
and the next person to ask "why not SQLite / why not modal keys / why not a
daemon" has to re-derive the argument from nothing.

- [Architecture](lnmsg-architecture.md) — the library it stands on, the
  driver seam that nearly forces the shape, process architecture, the TEA
  split, scriptability, and the library gaps.
- [The user interface](lnmsg-ui.md) — the input model, and what honesty
  about delivery looks like.
- [Conversation storage](lnmsg-storage.md) — SQLite, the schema, retention.
- [The mailbox and who you talk to](lnmsg-mailbox.md) — propagation-node
  policy, trust, contacts and names.

## 1. What the program is

A terminal client for reading and sending LXMF messages over Reticulum,
connected to a running `lnsd` or `rnsd` shared instance, the same way
`lnomad` connects.

It must talk to a propagation node. A propagation node is the mailbox that
holds messages addressed to a client that was not reachable. Without it, a
laptop that is closed for eight hours simply does not receive mail, and the
program is a toy. With it, the program is usable on hardware that is off
most of the time, which is the actual deployment.

Scope explicitly excluded: hosting a propagation node. `leviculum-lxmf`
carries both ends of the *client to node* codec exchange, but a host built
on those codecs still owns its own mailbox storage, stamp validation, links
and resources, because the crate performs no I/O
(`leviculum-lxmf/src/lib.rs:16-18`). What the crate does not implement at
all is the *node to node* direction — peer sync, peering keys and the
`/offer` path (`leviculum-lxmf/src/lib.rs:20-30`, Codeberg #209) — and the
two client modules repeat the exclusion in their own headers
(`leviculum-lxmf/src/router/propagation_runtime.rs:3-5`,
`leviculum-lxmf/src/propagation_client.rs:5-8`). A client cannot become a
node without new library work. That is a separate program and a separate
argument — and it now exists: the channel design record
([Public channels over LXMF](public-channels.md)) reopens hosting,
because the channel retrieval side only works on nodes we run.

### The name

The house convention is one `ln*` counterpart per reference tool
([Client tools](client-tools.md), "A counterpart for every reference
tool"), and the existing family is `lnsd`, `lnstatus`, `lncp`, `lnomad`.
There is no single reference tool to be a counterpart of: LXMF messaging in
the reference world lives inside NomadNet and Sideband, not in a standalone
utility.

**Decided 2026-08-10: `lnmsg`.** Rejected: `lnmail` promises mail while the
UI is chat (the [input-model decision](lnmsg-ui.md)); `lnchat` promises
chat while the protocol delivers mailbox behaviour on slow paths.
"Message" is the only word the protocol can always honour, and `lnmsg send`
in a cron job explains itself.

## 2. The ten decisions

| # | Question | Verdict | Where |
|---|---|---|---|
| 1 | Single process or daemon plus client | **C built as A first**: one process now, the router and store behind an interface so daemon mode is a wiring change. The `lnsd`-resident variant is rejected twice over. The core/frontend boundary is binding from day one, so a GUI is a frontend swap. | [Architecture](lnmsg-architecture.md) |
| 2 | Modal, always-insert, focus-follows-pane, or prefix | **C, focus-follows-pane**, with a command palette and no modes. `Enter` in compose sends — the quiet keyboard's one named exception — `Alt-Enter` newline, empty-buffer guard, `send_on_enter` switch for email style. `Ctrl-C` clears to a recoverable draft and never quits. | [UI](lnmsg-ui.md) |
| 3 | SQLite or a pure-Rust store | **SQLite**, via `rusqlite` with `bundled`. The musl static build was tried and works, FTS5 included. Identity-scoped schema, two timestamps, raw msgpack fields, attachments out of line. | [Storage](lnmsg-storage.md) |
| 4 | Default sync interval, adaptive or not | **Asymmetric adaptivity**: fifteen minutes as a hard never-exceeded upper bound, faster (about two minutes, decaying) after activity. Silent lengthening rejected as a breach of trust. | [Mailbox](lnmsg-mailbox.md) |
| 5 | Trust: three states or four, derived "suspicious"? | **Exactly three** — known, unknown, blocked. Trust follows from having named someone. No suspicious state: name collisions and name changes are normal mesh life, handled by display disambiguation. | [Mailbox](lnmsg-mailbox.md) |
| 6 | Delivery ledger: visible, keypress, or debug? | **One keypress away** in the per-message detail view, plus a one-cell coloured state glyph per message, all from one shared state table, with an ASCII fallback. | [UI](lnmsg-ui.md) |
| 7 | Does `lnmsg send` block? | **No.** Exit 0 means "queued cleanly" and claims nothing more. A success prints nothing (amended 2026-08-21: the ID came off stdout; it stays reachable as `LNMSG_ENQUEUED id=…` in the event log). `--wait` opts into blocking with timeout-is-not-failure exit codes; `lnmsg status <id>` answers later. | [Architecture](lnmsg-architecture.md) |
| 8 | Retention policy | **Text forever**, attachments under a total-bytes budget (about 500 MB by default, oldest evicted, the message row keeps name, hash and size and says so). Ledger rows share the message's retention. A per-conversation age cap is deliberately deferred. | [Storage](lnmsg-storage.md) |
| 9 | The name | **`lnmsg`.** | above |
| 10 | Close library gaps first or discover by building? | **Triage.** Gaps 1, 2, 3 and 8 — announce names, the propagated-awaiting-collection state, file-backed storage, `Display` on errors — close in one library wave before `lnmsg` starts. The other eight become issues met in build order. | [Architecture](lnmsg-architecture.md) |

## 3. Scale and honesty about hardware

This runs on machines from a Raspberry Pi upward, which sets a few hard
numbers.

**Memory.** The whole model plus the visible window, not the whole history.
`lnomad`'s eager full-page layout (`lnomad/src/tui.rs:910-934`) is
acceptable for a page and not for a conversation. Attachments in a
byte-budgeted cache, not a count-budgeted one
(`lnomad/src/image_cache.rs:11-14`).

**Disk.** SD cards die from writes. This argues against NomadNet's
file-per-message plus a full `.index` rewrite on every change, and for a
store that appends. It also argues for batching `persist()` rather than
calling it on every `PersistenceRequested`, at the cost of a bounded window
of loss.

**CPU.** Proof-of-work is the one unbounded computation in the system. On a
Pi it must run off the core lock and at low priority, and the UI must be
able to say "this will take a while" with a number that was measured on
that class of hardware.

**The core lock is the real budget.** 5 ms per hook call
(`PROCESSOR_TICK_BUDGET`, `leviculum-std/src/driver/processor.rs:172`),
against 3.2 ms to verify a 1 MiB message's signature per
[The core lock budget](core-lock-budget.md). A messenger doing anything
expensive inside the hook stalls the node's inbound path for every other
client of the shared instance. Everything that is not the router's own
state machine belongs on the far side of a queue.

**Airtime is the scarcest resource of all.** Every design choice on these
pages that trades bytes for clarity — a sync poll, an announce, a read
receipt we are not going to invent — is spending the one thing that cannot
be bought back.

## 4. Compatibility constraints

Non-negotiable, per the project's first priority.

- **No wire-format changes.** Everything on these pages is expressible in
  LXMF as it is. The delivery ledger, the collision warning, the cost
  estimate and the mailbox screen are all local views over data the
  protocol already carries.
- **No new fields.** If threading or replies are wanted, they use
  `FIELD_THREAD (0x08)`, `FIELD_REPLY_TO (0x30)` and
  `FIELD_REPLY_QUOTE (0x31)` with the reference's semantics
  (`reference/LXMF/LXMF/LXMF.py:15`, `:23-24`), not a private encoding.
- **Unknown fields round-trip**, as the library already guarantees
  (`leviculum-lxmf/src/message.rs:5-8`). A reply composed by us to a
  message from a newer client must not silently drop what we did not
  understand.
- **Names on the wire are bytes**, and we sanitise for display without
  altering what we forward.
- **Interoperability is tested, not assumed.** Per the project's test
  discipline, this program needs interop tests against real Python LXMF
  peers and a real propagation node, positive and negative, before it is
  called finished. `leviculum-lxmf-node` exists precisely to make such A/B
  comparisons possible with one driver
  (`leviculum-lxmf-node/src/lib.rs:12-18`), and the same trick applies
  here.

## 5. Provenance

### Prior art, condensed

Two programs were read end to end before any of this was decided, and most
of the arguments on the other four pages are arguments with one of them.

**NomadNet.** Storage is one directory per conversation named by the peer
hash and one file per message named by the LXM hash, with `unread` and
`failed` side-car counters and a msgpack `.index` that caches timestamp,
state, title and *the full content* of every message
(NomadNet's `Conversation.py`, lines 62-71, 120, 236, 93-109 and 944-960).
Attachments are stripped out into `storage/attachments/<hash>/` with a
manifest and sanitised names, which is good. Everything else about the
storage is not: `scan_storage()` does a full `listdir` and constructs a
`ConversationMessage` for every file on every call, and
`update_message_widgets()` rebuilds every widget and replaces the whole
listbox on every change (`Conversations.py`, lines 2254-2291). NomadNet
commit `7bc6911` exists specifically because calling the conversation list
on every announce caused file-descriptor starvation during announce storms.
There is no pruning, no paging and no search anywhere.

Its chat pane is a two-column layout with the editor in the frame footer
and initial focus on it. Composition is an inline `MessageEdit` with a
readline mixin; there is no `$EDITOR` integration, and send is `Ctrl-D`.
Every command is Ctrl-modified, so plain typing always reaches the edit
box, at the cost of an exhausted and context-overloaded Ctrl namespace:
`Ctrl-X` is "delete conversation" in the list and "clear history" in the
body, `Ctrl-U` is "ingest URI" and "purge failed", `Ctrl-P` is "my QR" and
"paper message". Delivery state is glyph and colour only: the distinction
between `SENT`, `DELIVERED` and propagated-`SENT` genuinely exists and gets
three different styles, but it is never stated in words, and propagated
shares a style with paper messages. Two honest touches: signature failure
is rendered in plain English ("Unknown Origin", "Invalid Signature"), and
on load a message stuck mid-flight that is no longer in the router's
pending set is forced to `FAILED`.

Identity handling is where NomadNet is genuinely ahead of everything else:
four trust levels with real UI consequences, and, uniquely, duplicate
display names raise `WARNING`. Notification is a terminal bell, literally
`sys.stdout.write("\a")`.

**columba.** A Room database, fifteen entities, and two schema decisions
worth taking. Every row is scoped by `identityHash` with composite primary
and foreign keys, so multiple local identities coexist with cascading
isolation. And `receivedAt` (local clock) is stored separately from
`timestamp` (sender clock), with sorting on
`COALESCE(receivedAt, timestamp)` and an explicit comment that this is
immune to sender clock skew. NomadNet has the same problem and solves it
worse, as a user-toggled sort mode on `Ctrl-O`. Paging3 with a `DESC` query
and a reversed layout is the right answer to NomadNet's rebuild-everything
problem.

Its propagation handling adds three things NomadNet lacks: a
user-configurable sync interval, real failover to an alternative relay when
one dies, and a structured `SyncResult` / `SyncProgress` sum type where
manual syncs report loudly and periodic ones stay silent. The relay is
modelled as a contact with an `isMyRelay` flag, which is a nice way to make
the relationship visible. Contact status is a persisted enum — `ACTIVE`,
`PENDING_IDENTITY`, `UNRESOLVED` — rather than a live key lookup, and name
resolution has a documented precedence: in-memory cache, then the user's
own nickname, then the announced name, then the stored conversation name.
User nickname beating announced name is exactly right.

What columba gets wrong: it has **no trust model at all**. A search for
`trustLevel` / `isTrusted` across its app, domain and data modules returns
nothing outside tests, there is no duplicate-name detection, and
consequently no trust gate on relay auto-selection. It will happily adopt
the nearest stranger's relay. Its delivery display also collapses `sent`
and `propagated` into the same single check mark, which is precisely the
distinction that matters most on a delay-tolerant network. Its per-message
detail screen, showing delivery method with a one-sentence explanation plus
hop count, interface, RSSI and SNR, is the best idea in either program and
is trivial to do in a terminal.

Neither program prunes history.

### The citation convention on these pages

Claims about existing code carry `file:line`. Where something could not be
verified, it says so.

In-tree citations were re-pinned against this tree when the concept was
promoted (2026-08-10); the draft had been written days earlier and the
`leviculum-lxmf` crate had moved substantially under it. The citation guard
(`leviculum-std/tests/doc_citations.rs`) resolves every one of them.

Claims about the vendored references use the same form under `reference/`,
for example `reference/LXMF/LXMF/LXMRouter.py:38`, which is what every
other page in this book does.

**NomadNet and columba are not vendored here** and are not in this repo, so
the guard cannot resolve a citation into either and would report one as a
file that had been deleted or renamed. Their line references are therefore
written with the file name backticked and the lines outside it — NomadNet's
`Conversation.py`, lines 62-71 — which says exactly as much and does not
claim a path this tree holds. Every such reference names NomadNet or
columba in the same sentence, so a reader always knows which tree to open.
