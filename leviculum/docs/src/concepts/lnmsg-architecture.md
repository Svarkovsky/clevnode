# lnmsg: architecture

Part of the [lnmsg design record](lnmsg.md). This page carries the ground
truth the design stands on — `lnomad` and `leviculum-lxmf` — the driver
seam that nearly forces the architecture, the process and event-loop
decisions, scriptability, and the triage of what the library does not yet
expose.

## 1. `lnomad`, and one correction to the premise

`lnomad` is described as supporting "Emacs keybindings, vi keybindings,
Firefox keybindings and Firefox mouse behaviour, all at the same time".
That is the observable behaviour, but it is not implemented as four
schemes. It is one keymap with three resolution mechanisms, and only one of
them is table-driven.

**What is table-driven.** `SCROLL_KEYS` (`lnomad/src/tui.rs:3159-3291`) is
a static table of `ScrollKey { keys, desc, chords }` where each
`ScrollChord { code, mods, cmd }` carries a modifier class
(`ScrollMods::{Any, Plain, Ctrl, Alt}`, `lnomad/src/tui.rs:3128-3135`). One
row carries the vi and the emacs and the arrow spelling of the same motion
side by side:

```rust
keys: "j / k   ↓ / ↑   Ctrl-n / Ctrl-p",   // lnomad/src/tui.rs:3161
desc: "scroll a line",
```

Resolution is a linear scan, first match wins
(`key_to_scroll`, `lnomad/src/tui.rs:3293-3311`). The table is read by both
the key handler and the help overlay (`lnomad/src/tui.rs:4669-4680`), and
the doc comment says that is deliberate: "the SINGLE source of truth read
by BOTH" (`lnomad/src/tui.rs:3147-3151`).

**What is not.** Everything else is a hand-written `if`-chain in
`update_browse_key` (`lnomad/src/tui.rs:1810-1957`): roughly twenty
sequential `if key.code == ... { return ...; }` statements. There is no
binding map, no user-configurable keymap, no keybinding config file. The
help overlay's non-scroll groups are a second, unlinked static list
(`lnomad/src/tui.rs:4687-4788`) that can silently drift from the handler.

**How the conflicts are actually resolved.** Three layers, in this order.

1. *Global escapes*, before any mode dispatch
   (`update_key`, `lnomad/src/tui.rs:1566-1606`): any key dismisses the
   toast; `Ctrl-C` quits from anywhere; an open help overlay swallows
   everything; an open places panel takes over.
2. *Mode gating*. `Mode::{Browse, Address, Hint, Search, Field}`
   (`lnomad/src/tui.rs:292-311`), each with its own handler. Text modes
   forward unclaimed keys to a `tui_input::Input` editor. `Mode::Field`
   uses a whitelist rather than a catch-all so that "field editing never
   leaks into browse hotkeys" (`lnomad/src/tui.rs:1614`, whitelist at
   `:1643-1652`).
3. *Modifier discrimination*. Nearly every single-letter binding is guarded
   `&& !ctrl && !alt`, which is what lets `f` (hint mode) and `Ctrl-f`
   (page down), `d` (places) and `Ctrl-d` (half page), `n` (next match) and
   `Ctrl-n` (line down), `g` (top) and `Ctrl-g` (cancel) all coexist.

The ordering is load-bearing. In browse mode `key_to_scroll` is consulted
*last* (`lnomad/src/tui.rs:1951-1954`), so single-letter commands claim
their keys first and `j`/`k`/`Space` reach the scroll table only because
nothing above claims them. In the places panel the order is *inverted*
(`lnomad/src/tui.rs:2145`) with a comment explaining why: there `Ctrl-d`
must be a half-page motion, not the `d` that closes the panel.

There is one further principle worth carrying over verbatim. Bare `r` is
deliberately left unbound, because "a mesh reload is expensive and single
letters are reserved for cheap local actions"
(`lnomad/src/tui.rs:1930-1933`); reload requires `R`, `Ctrl-R` or `F5`.
That is a cost-aware keymap, and a messenger sends over the same radios.

**Architecture.** Elm-style with an explicit effect list, all in
`lnomad/src/tui.rs`: `Model` at `:680-838` (`#[derive(Clone, Debug,
Default)]` at `:679`), `AppEvent` at `:1197-1264`, `Effect` at `:592-646`,
`update(&mut Model, AppEvent) -> Vec<Effect>` at `:1271-1381`,
`view(&Model, &ImageStore, &mut Frame)` at `:3532-3571`, and a single
effect interpreter `run_effects` at `:5783-5889`. `update` mutates rather
than returning a new model, and effects are plain data, not closures. That
combination is what makes the 245 in-file unit tests possible: build a
`Model`, feed a synthetic `AppEvent::Key`, assert on the model and on the
returned `Vec<Effect>`, with no IO anywhere (`lnomad/src/tui.rs:6228`
onwards; helpers at `:6238-6251`). The view is tested against
`ratatui::backend::TestBackend` (`:6232`, `:7055`), and the `--print` path
has byte-identical golden files (`lnomad/tests/render_golden.rs:18-37`).

**Unsolicited inbound events already exist.** This matters more than
anything else for a messenger, and the answer is yes.
`AppEvent::NodeDiscovered` (`lnomad/src/tui.rs:1264`) arrives from
announces with no user action. The chain is: an announce sink installed
before the session is shared so nothing is missed at startup
(`set_announce_sink`, `lnomad/src/fetch.rs:213-215`, wiring comment at
`lnomad/src/tui.rs:5995-5998`), a non-blocking unbounded send on every
recorded announce (`note_announce`, `lnomad/src/fetch.rs:355-378`), a
dedicated background task parked on the shared session in 250 ms lock
slices (`spawn_discovery`, `lnomad/src/tui.rs:5721-5762`), and a
`tokio::select!` arm that folds the result into the model
(`lnomad/src/tui.rs:6154-6156`). The main loop has five arms
(`lnomad/src/tui.rs:6058-6161`), and the timer arm is conditionally enabled
(`, if animate` at `:6157`, driven by `needs_tick()` at `:1018-1021`) so an
idle browser does not wake eight times a second.

**Persistence.** Three small files under
`${XDG_CONFIG_HOME:-~/.config}/lnomad/`: `bookmarks.toml`, `identify.toml`,
and a binary `identity`. Everything else is RAM. The write path is
`fs::write` with no atomic rename, no fsync, and errors deliberately
ignored (`lnomad/src/bookmarks.rs:124-130`, effect handler at
`lnomad/src/tui.rs:5879-5885`), and load treats corrupt exactly like
missing (`lnomad/src/bookmarks.rs:116-121`). For bookmarks that is a
defensible trade. For a message store it is data loss.

`load_or_create` (`lnomad/src/identity.rs:39-53`) silently mints a fresh
identity when the stored one fails to decode. For a browser, whose identity
is disposable, that is right. For a messenger, whose identity *is* the
user's address, silently replacing it breaks every contact's address book
with no warning. That default must be inverted.

**Two caches worth copying.** The page cache stores the parsed document
rather than the laid-out page, because layout depends on width and theme
(`lnomad/src/page_cache.rs:10-13`). The image cache is bounded by *bytes*
rather than count, and the reasoning generalises directly to attachments:
"a cache of 'the last fifty pictures' says nothing about how much memory a
browser is holding, and pictures differ in size by three orders of
magnitude" (`lnomad/src/image_cache.rs:11-14`).

**Rendering.** One layout core, two sinks. `layout_blocks`
(`lnomad/src/render.rs:192-217`) produces `Vec<RLine>` where `RLine` is a
vector of `StyledChar { ch, st, link, field }`
(`lnomad/src/render.rs:340-361`): already wrapped, aligned and indented,
one `RLine` per output row, every cell carrying its resolved style and its
owning link index. That IR feeds either `to_ratatui_text`
(`lnomad/src/tui.rs:5143-5162`) for the TUI or `emit_ansi`
(`lnomad/src/render.rs:224-231`) for `--print`. Scrolling is a slice, not a
widget scroll, because the page is pre-wrapped
(`lnomad/src/tui.rs:3782-3789`), and there is one scroll rule shared by
every scrollable window (`scrolled`, `lnomad/src/tui.rs:274-289`).

Two rendering caveats. Wrapping compares `cur.len() > width`, i.e.
character count rather than display width (`wrap`,
`lnomad/src/render.rs:848-875`), which will overflow on CJK and emoji. No
test covering that was found, so whether it is a known limitation or an
oversight is unclear. And the whole page is laid out eagerly on every
relayout (`lnomad/src/tui.rs:910-934`), including on every keystroke in a
form field (`:1657-1660`).

**Scriptability, and the absence of settings.** `--print` fetches, renders
and prints once (`print_once`, `lnomad/src/browser.rs:133-141`). Output is
raw ANSI page text and nothing else: no link markers, no legend, and with
`--no-color` links are indistinguishable from body text
(`lnomad/src/render.rs:143-146`). There is no JSON output anywhere in the
crate: `serde_json` is not a dependency. Non-interactive detection is
automatic: `interactive = !args.print && stdin().is_terminal() &&
stdout().is_terminal()` (`lnomad/src/main.rs:167-168`), so piping never
blocks on the UI. Exit codes: 0 success, 1 operational failure, 2 argument
or URL error (`lnomad/src/main.rs:174`, `:188`, `:222`, `:238`).

`lnomad` has no settings file at all. `--config` points at the *Reticulum*
config directory; the `lnomad/` directory holds only data. The theme is
auto-detected via OSC 11 before raw mode is entered
(`lnomad/src/tui.rs:5956-5963`) and toggled at runtime with `t`; theme
colours are hard-coded (`lnomad/src/theme.rs:112-193`).

**The handoff that already exists.** `lnomad` recognises `lxmf@<hash>`
links, and because it has no composer it copies the address to the
clipboard and says so in a toast (`follow_link`,
`lnomad/src/tui.rs:2768-2777`). The messenger is the natural target of that
handoff, and wiring the two together is an explicit goal.

## 2. `leviculum-lxmf`: what it gives and what it does not

Three layers, all sans-IO: `NodeCore` (Reticulum transport, owned by the
app), `LxmfNode` (`leviculum-lxmf/src/node.rs:351`, the `lxmf.delivery`
destination adapter), and `LxmfRouter`
(`leviculum-lxmf/src/router.rs:442`, the queue, retry scheduler, stamp and
ticket policy, dedup caches and propagation client). The application builds
on `LxmfRouter` and owns both it and the core; the router never owns the
core, every method takes it as a parameter.

Note that `LxmfRouter`, `RouterEvent`, `RouterOutput`, `RouterConfig` and
`MessageState` are *not* re-exported at the crate root — the crate root
exports only `BuiltResource`, `DeliveryStampRequest`, `InboundStampRequest`,
`PendingResourceBuild` and `PropagationStampRequest` from that module
(`leviculum-lxmf/src/lib.rs:73-76`) — so they are reachable as
`leviculum_lxmf::router::*` only.

### Events are return values, not a channel

```rust
#[must_use]
pub struct RouterOutput {          // leviculum-lxmf/src/router.rs:300-303
    pub core: TickOutput,
    pub events: Vec<RouterEvent>,
}
```

Every router method that can produce work returns this. There is no
callback and no channel. The library never drops an event, but it never
retains one either: if the application drops a `RouterOutput`, those events
are gone. `#[must_use]` on both `RouterOutput` and `TickOutput`
(`leviculum-core/src/transport.rs:281`) is the only safety net, and
`TickOutput`'s own doc says dropping it "silently loses outbound packets
and application events" (`leviculum-core/src/transport.rs:262-264`).

There is a re-entrancy obligation that is easy to miss and fatal to get
wrong: `RouterOutput.core.events` contains `NodeEvent`s that must be fed
*back* into `router.handle_event()`, recursively, until the worklist
drains. This is exactly Codeberg #204's subject. `leviculum-lxmf-node`
implements it with a bounded worklist and says why the bound is the
consumer's choice (`MAX_ABSORB_ROUNDS`,
`leviculum-lxmf-node/src/processor.rs:74-84`, `absorb` at `:392-394`). A
client that forgets this will silently never see incoming messages.

### `RouterEvent`

Fifteen variants (`leviculum-lxmf/src/router.rs:268-296`):
`MessageQueued`, `MessageState { message_id, state }`, `MessageReceived`,
`InboundRejected`, `DirectLinkEstablished`, `Duplicate`,
`InvalidSignature`, `InvalidStamp`, `ResourceBuildPending`, `StampPending`,
`InboundStampPending`, `PropagationStampPending`, `PropagationSyncState`,
`PropagationSyncComplete`, `PersistenceRequested`.

What is missing is as informative as what is there. There is no announce
event: `LxmfNodeEvent::PeerAnnounced` carries the destination hash only,
with app data discarded (`leviculum-lxmf/src/node.rs:133-135`,
`:746-754`), and `handle_node_event` does not forward it at all — it falls
into `_ => {}` (`leviculum-lxmf/src/router.rs:1322`). The router does
decode the delivery announce but keeps only `stamp_cost` and
`compression_supported`, discarding the display name
(`leviculum-lxmf/src/router.rs:1173-1183`). **Display-name learning is
entirely the client's job**, from raw `NodeEvent::AnnounceReceived`.

There is also no event for `Sending`, for `Outbound`, or for progress: the
router folds `LxmfNodeEvent::Progress` into `OutboundEntry::progress`
without emitting anything (`leviculum-lxmf/src/router.rs:1306-1318`), so
progress must be polled through `outbound()`
(`leviculum-lxmf/src/router.rs:672`).

### `MessageState` and what it honestly means

```rust
pub enum MessageState {          // leviculum-lxmf/src/router.rs:62-71
    Generating = 0x00, Outbound = 0x01, Sending = 0x02, Sent = 0x04,
    Delivered = 0x08, Rejected = 0xfd, Cancelled = 0xfe, Failed = 0xff,
}
```

Discriminants are the Python `LXMessage` constants. Four traps:

1. **`Generating` is dead.** It is only ever produced by snapshot decoding
   (`leviculum-lxmf/src/router.rs:2333`); nothing assigns it.
2. **`Sent` means two different things and never applies to direct
   delivery.** For opportunistic messages it means the packet was handed to
   Reticulum unproven, and the message is still queued and still retryable
   (`leviculum-lxmf/src/router.rs:1291-1305`). For propagated messages it
   means the propagation node accepted the upload, and the entry is
   *deleted* (`leviculum-lxmf/src/router/propagation_runtime.rs:378-385`).
   Direct delivery goes `Outbound -> Sending -> Delivered | Rejected |
   Failed` and never passes through `Sent`, because the `Submitted` handler
   matches only `DeliveryMethod::Opportunistic`
   (`leviculum-lxmf/src/router.rs:1359-1363`).
3. **`Delivered` is a Reticulum transport proof, not an application
   receipt.** It comes from `PacketDeliveryConfirmed` /
   `LinkDeliveryConfirmed` (`leviculum-lxmf/src/node.rs:1115-1137`) or from
   `ResourceCompleted { is_sender: true }`
   (`leviculum-lxmf/src/node.rs:1009-1020`). It proves the bytes arrived at
   the destination identity. It does not prove an LXMF client parsed them
   and it certainly does not prove a human read them. There is no
   read-receipt field in LXMF at all
   (`leviculum-lxmf/src/constants.rs:20-46`).
4. **`Rejected` is ambiguous.** It means either "the receiver cancelled the
   Resource transfer" (`leviculum-lxmf/src/router.rs:1249-1262`) or "the
   propagation node refused the upload for an insufficient stamp"
   (`leviculum-lxmf/src/router/propagation_runtime.rs:410-422`), and the
   event alone cannot distinguish them.

And one omission that shapes the whole UI: **there is no "propagated but
not yet collected" state.** A propagated message reaches `Sent`, its queue
entry is removed, and from then on it is indistinguishable from a message
that vanished.

Terminal states remove the entry from the outbound map
(`remove_outbound`, `leviculum-lxmf/src/router.rs:844-847`; call sites at
`:859`, `:1298`, `:1322` and
`leviculum-lxmf/src/router/propagation_runtime.rs:887`). If the client does
not capture the `Message` at `enqueue` time it cannot render its own sent
message afterwards, and it cannot offer a retry button.
`MAX_DELIVERY_ATTEMPTS` is 5 (`leviculum-lxmf/src/router.rs:46`).

### Propagation: what the router does, and what it refuses to do

Setup requires the client to mint a second `lxmf.propagation` destination
via `PropagationTransport::destination`
(`leviculum-lxmf/src/propagation_client.rs:282-292`),
register it, and hand it to `enable_propagation_client`
(`leviculum-lxmf/src/router.rs:577`); the transport identity must equal the
router's or you get `RouterError::IdentityMismatch`
(`leviculum-lxmf/src/router.rs:582-584`).

Node discovery is automatic from announces (`remember_announce`,
`leviculum-lxmf/src/propagation_client.rs:384-400`, driven from the
announce arm at `:733-742`), and the decoded announce carries `enabled`,
`transfer_limit_kb`, `sync_limit_kb`, `stamp_cost`, `peering_cost` and
`metadata` (`PropagationNodeAnnounce`,
`leviculum-lxmf/src/propagation.rs:513-525`), all of which are directly
displayable. `select_outbound_propagation_node` with `None` auto-ranks by
route, hops, peering cost and stamp cost
(`leviculum-lxmf/src/router/propagation_runtime.rs:1153-1188`).

Once a sync starts, everything is automatic: path request, link, identify,
list request, want/have partitioning, download, acknowledge and purge
(`begin_list_request`,
`leviculum-lxmf/src/router/propagation_runtime.rs:451-543`). The observable
state machine is `PropagationClientState`
(`leviculum-lxmf/src/router/propagation_runtime.rs:60-75`),
wire-compatible with Python's `PR_*` constants: `Idle`, `PathRequested`,
`LinkEstablishing`, `LinkEstablished`, `RequestSent`, `Receiving`,
`ResponseReceived`, `Complete`, `NoPath`, `LinkFailed`, `TransferFailed`,
`NoIdentity`, `NoAccess`, `Failed`. `ResponseReceived` is never assigned in
practice. There is automatic failover to another reachable node when the
selected one loses its route
(`leviculum-lxmf/src/router/propagation_runtime.rs:817-838`).

What the router will not do:

- **It never schedules a sync.** `request_messages_from_propagation_node`
  (`leviculum-lxmf/src/router/propagation_runtime.rs:1323`) must be called
  by the application every time. `PropagationClientConfig` has three fields
  and none of them is an interval
  (`leviculum-lxmf/src/router/propagation_runtime.rs:35-45`), and
  `next_deadline()` returns `None` in every state except `PathRequested`
  (`leviculum-lxmf/src/router/propagation_runtime.rs:1134-1140`).
- **It does not persist known propagation nodes.** They live in an
  in-memory map (`known_nodes`,
  `leviculum-lxmf/src/propagation_client.rs:267`) and are absent from the
  router snapshot (`snapshot`, `leviculum-lxmf/src/router.rs:1896-1913`).
  The client must persist and replay them via
  `restore_known_propagation_node`
  (`leviculum-lxmf/src/router/propagation_runtime.rs:1309`). The selected
  node is not snapshotted either.
- **It does not clamp the transfer limit against the node's advertised
  one.** The download request carries the local
  `delivery_transfer_limit_kb` (default 1000) regardless of what the node
  announced (`leviculum-lxmf/src/router/propagation_runtime.rs:525-530`).

Default `retain_synced_on_node` is `false`
(`leviculum-lxmf/src/router/propagation_runtime.rs:50`), meaning the client
tells the node to purge what it has collected. That is a user-visible
policy decision disguised as a config default, and
[the mailbox page](lnmsg-mailbox.md) argues it should be surfaced.

The reference holds messages for `MESSAGE_EXPIRY = 30*24*60*60`, i.e.
thirty days (`reference/LXMF/LXMF/LXMRouter.py:38`).

### Storage is a bare key/value trait

```rust
pub trait LxmfStorage {          // leviculum-lxmf/src/storage.rs:18-26
    fn load(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError>;
    fn store(&mut self, key: &[u8], value: &[u8]) -> Result<(), StorageError>;
    fn remove(&mut self, key: &[u8]) -> Result<(), StorageError>;
    fn keys(&self, prefix: &[u8]) -> Result<Vec<Vec<u8>>, StorageError>;
    fn flush(&mut self) -> Result<(), StorageError> { Ok(()) }
}
```

There is no conversation, thread, contact or history concept in it. Two
implementations exist, both in that file: `MemoryLxmfStorage`
(`leviculum-lxmf/src/storage.rs:42`) and `NoLxmfStorage`
(`leviculum-lxmf/src/storage.rs:116`). The file-backed one is `FileLxmfStorage`
(`leviculum-std/src/file_lxmf_store.rs:27`), in the std crate because the LXMF
crate is `no_std`.

The router writes exactly one key, `b"lxmf/router-state"`
(`ROUTER_STATE_KEY`, `leviculum-lxmf/src/router.rs:54`), holding the
outbound queue, delivered and processed ID windows, stamp costs, tickets
and the ignore set (`leviculum-lxmf/src/router.rs:1896-1913`). A client
should stay off the `lxmf/` prefix and is otherwise free.

Restore resets every queued message to `Outbound` with
`next_attempt_ms = 0` and `progress = 0.01`
(`leviculum-lxmf/src/router.rs:1881-1884`), because in-flight correlation
is expressed in a process-local monotonic clock that does not survive a
restart. A UI therefore cannot show a stable "sending" progress across
restarts, and must not pretend to.

### Features a UI could surface

- **Attachments** (`leviculum-lxmf/src/attachments.rs`): files, one image,
  one audio clip, as `MessageAttachments::into_fields()`
  (`leviculum-lxmf/src/attachments.rs:57`) / `from_fields()`
  (`leviculum-lxmf/src/attachments.rs:86`). Attachments are inline bytes in
  the message, so anything with a real attachment exceeds the packet MDU
  and forces link or Resource delivery (`representation`,
  `leviculum-lxmf/src/node.rs:488-516`).
- **Paper messages** (`leviculum-lxmf/src/paper.rs`): a message encrypted
  to a destination and rendered as an `lxm://` base64 URI (`to_uri`,
  `leviculum-lxmf/src/paper.rs:172`), capped at `PAPER_MDU = 2210` bytes
  (`leviculum-lxmf/src/constants.rs:9`). Ingest via
  `router.ingest_paper(uri)`
  (`ingest_paper`, `leviculum-lxmf/src/router/paper_runtime.rs:17`). No QR
  generation exists; that is the client's job.
- **Tickets** (`leviculum-lxmf/src/ticket.rs`): a 16-byte secret you issue
  to a contact so their future messages skip proof-of-work. Mostly
  invisible and automatic: received tickets are remembered from any
  signature-valid inbound message — `remember_verified_ticket`
  (`leviculum-lxmf/src/router.rs:1455`) — and applied when a message is
  enqueued (`leviculum-lxmf/src/router.rs:791`). Expiry 21 days, renew at 14,
  minimum one day between issuances to the same peer
  (`leviculum-lxmf/src/constants.rs:34-37`). `issue_ticket_field` refuses
  with `RouterError::NoWallClock` when the node's clock is implausible
  (`leviculum-lxmf/src/router.rs:652-653`), and can also legitimately
  return `Ok((None, _))` when rate-limited
  (`leviculum-lxmf/src/router.rs:670`). A UI has to distinguish "granted",
  "not yet, try tomorrow" and "cannot, no clock".
- **Stamps** (`leviculum-lxmf/src/stamp.rs`): proof-of-work over the
  message ID, cost being required leading zero bits, so expected work is
  2^cost hashes plus a workblock expansion of 3000 rounds
  (`WORKBLOCK_EXPAND_ROUNDS`, `leviculum-lxmf/src/constants.rs:39`). Costs
  above about 40 bits are described in-tree as "already unreachable in
  practice" (`leviculum-lxmf/src/router.rs:1109-1110`). No wall-clock
  benchmark exists in the crate and none was run for this document, so any
  UI estimate of mining time must be measured first, not guessed. There is
  no cancellation and no deadline: `generate` loops until it succeeds
  (`leviculum-lxmf/src/stamp.rs:171-182`), and `StampError::Cancelled`
  exists but is never constructed (`leviculum-lxmf/src/stamp.rs:24`).

### Fields with constants but no codec

`leviculum-lxmf/src/constants.rs:20-46` declares the full LXMF field set
including `FIELD_THREAD (0x08)`, `FIELD_RENDERER (0x0F)`,
`FIELD_REPLY_TO (0x30)`, `FIELD_REPLY_QUOTE (0x31)`,
`FIELD_REACTION (0x40)` and `FIELD_COMMENT (0x41)`, but only files, image
and audio have typed codecs. Unknown fields round-trip byte-for-byte
(`leviculum-lxmf/src/message.rs:5-8`), so nothing is lost, but a client
wanting replies, threads, reactions or renderer-aware display must
hand-roll the msgpack via the exported `msgpack` module.

`RENDERER_MICRON = 0x01` (`reference/LXMF/LXMF/LXMF.py:100`) is interesting
here: `leviculum-micron` already parses micron into a document model
(`leviculum-micron/src/lib.rs:25-27`) and `lnomad` already renders that
model. A messenger in this workspace can honour `FIELD_RENDERER` almost for
free, which no other terminal LXMF client does.

## 3. The driver seam, which nearly forces the architecture

An LXMF client cannot be fed from `leviculum-std`'s public event stream.
The reason is documented at the seam itself: the tap sits on
`output.events` inside `dispatch_output`, *before* the event sink
classifies, and seven of the event types LXMF needs, including
`PacketReceived` and `LinkDataReceived`, are `EventClass::Data` and
therefore droppable under load. A processor fed from `take_event_receiver`
"would silently lose inbound messages with nothing underneath to retransmit
them" (`leviculum-std/src/driver/processor.rs:182-190`, "Where the events
come from").

So the messenger must register a `CoreProcessor`
(`leviculum-std/src/driver/processor.rs:262-290`) on the builder, and the
LXMF router lives inside the driver's tick, under the core mutex. That
carries hard obligations:

- Both hooks run with a non-reentrant mutex held. The processor may not own
  a handle to the node it runs inside; roughly forty synchronous `pub fn`s
  on `ReticulumNode` open with a lock and one of them in a hook body
  deadlocks the node in ordinary safe code.
- Every side effect must be a non-blocking queue push.
  `leviculum-lxmf-node` does exactly this: stdout lines, stderr lines,
  proof-of-work jobs and shutdown are all channel sends
  (`leviculum-lxmf-node/src/processor.rs:15-30`).
- `PROCESSOR_TICK_BUDGET` is 5 ms per hook call
  (`leviculum-std/src/driver/processor.rs:172`), reported rather than
  enforced. Message packing costs about 0.8 ms and unpacking with signature
  verification about 3.2 ms for 1 MiB, per
  [The core lock budget](core-lock-budget.md). `NodeCore::send_resource`
  (`leviculum-core/src/node/mod.rs:1560`) must not be called from a hook:
  141 ms under the lock for 1 MiB.
- The processor needs its own periodic slot to drain its command queue,
  because an event tap can never initiate anything. `leviculum-lxmf-node`
  uses 200 ms (`POLL_INTERVAL_MS`,
  `leviculum-lxmf-node/src/processor.rs:72`).

This is a strong constraint and a gift at the same time: it means the
"model" that talks to the network is a synchronous state machine with a
queue on either side, which is exactly the shape that tests well.

## 4. Decision: process architecture

### Options

**A. One process. TUI plus an in-driver `CoreProcessor`.** The binary
builds a `ReticulumNode` as a shared-instance client with
`core_processor(...)` installed, exactly as `leviculum-lxmf-node` does
(`leviculum-lxmf-node/src/main.rs:170-177`). The processor owns the
`LxmfRouter`; the TUI owns the model. They talk over two unbounded
channels.

*For*: one binary, one config, no IPC to design, matches `lnomad`'s
deployment shape. *Against*: mail is only received while the TUI is
running. Closing the terminal stops collecting.

**B. Two processes. A headless daemon plus a thin TUI client.** A `lnmsgd`
holds the router and the store and exposes a local socket; the TUI is a
view onto it. *For*: mail arrives while the UI is closed, several front
ends can attach, and the store has one writer. *Against*: an entire IPC
protocol, a second daemon on a system that already runs `lnsd`, and a
second thing to package and supervise.

**C. One binary, two modes.** `lnmsg` with a `--daemon` flag, and the TUI
attaching to a running daemon if there is one and otherwise running the
router itself. *For*: option A's simplicity on day one, option B's
availability when the user asks for it. *Against*: two code paths for every
operation, and the temptation to test only one.

### Decision (2026-08-08)

**C, built as A first.** Start with a single process, but put the router
and the store behind an interface from the beginning so that the daemon
mode is a wiring change rather than a rewrite. Whether the daemon mode is
ever built is decided empirically: if syncing with a propagation node on
start plus every N minutes proves sufficient in the mesh we care about, A
alone stays. The reference retention default of thirty days
(`reference/LXMF/LXMF/LXMRouter.py:38`) suggests it might. This aligns with
the standing decision that propagation nodes, not client uptime, are the
answer to offline delivery.

The `lnsd`-resident variant — daemon mode as a `CoreProcessor` registered
inside `lnsd` — is **rejected**, twice over: it would put LXMF knowledge
into the transport daemon, which the Codeberg #196 seam was explicitly
designed to avoid, and it contradicts the standing rule that client
programs do not merge into `lnsd` (there will be more clients than this
one).

### Requirement: the core must not know it has a terminal

Decided 2026-08-08, and binding for `lnomad` too: the messenger will grow
other frontends on other platforms later — a GUI is expected — and that
must be a frontend swap, not a rework.

Concretely, the crate splits into two layers with a hard boundary:

- **`lnmsg-core`** (or a module boundary with the same discipline until a
  crate split is warranted): the model, `update`, effects, the store, the
  router glue, sync scheduling, trust, delivery bookkeeping. This layer
  never imports crossterm, ratatui, or any terminal type. Everything in it
  is driven by `AppEvent` in and `Effect` out, and is testable headless.
- **The TUI frontend**: rendering, key mapping, terminal lifecycle. It
  translates terminal events into `AppEvent`s and draws the model. A GUI
  frontend later is a second translator and a second renderer over the same
  core — no change to the core's types.

The TEA split below is what makes this cheap: the discipline is not a new
architecture, it is refusing to let the existing one leak. The test for the
boundary is mechanical and should exist from day one: the core compiles
without the TUI dependency tree (feature gate or crate split), and the
headless test suite drives complete user stories through `AppEvent`s alone.

For `lnomad` the same requirement holds as a future refactor: its TEA split
already keeps the model headless-testable, but model, update and view live
in one 11,873-line file with crossterm types reachable throughout. When
`lnomad` next gets substantial work, the same core/frontend boundary is
carved there. Tracked as its own issue, not as part of this program.

## 5. Decision: the event loop and the TEA split

`lnomad`'s split survives contact with a messenger with one change.

The shape that follows from the driver seam is three layers, not two:

```
  crossterm events ──┐
  router events    ──┼──> AppEvent ──> update(&mut Model) ──> Vec<Effect>
  timer            ──┘                                              │
                                                                    v
                                                            run_effects
                                                                    │
                                          Command queue ────────────┘
                                                    │
                                                    v
                              CoreProcessor::on_tick / on_event
                                (LxmfRouter, under the core lock)
                                                    │
                                          RouterEvent queue
                                                    │
                                                    └──> AppEvent
```

The processor is not part of the TEA model. It is a second, synchronous
state machine on the far side of two queues, and it is testable on its own
terms without a terminal, exactly as `leviculum-lxmf-node` is.

Three specific things `lnomad` does that must change:

1. **Bottom-anchored scrolling with a pinned flag.** `lnomad`'s `scroll` is
   the index of the top visible line (`lnomad/src/tui.rs:274-289`). A
   message list wants a "pinned to bottom" boolean so an inbound message
   appends without yanking the viewport out from under a user who has
   scrolled up. NomadNet gets this wrong: it resets to the bottom on every
   refresh (NomadNet's `Conversations.py`, line 2287).
2. **Windowed layout.** `lnomad` re-lays out the whole page on every
   relayout (`lnomad/src/tui.rs:910-934`). A ten-thousand-message
   conversation must not do that, and a compose buffer must not trigger it
   per keystroke. Lay out the visible window plus a margin, and cache per
   message keyed by `(message_id, width, theme)`.
3. **The timer must run.** `lnomad` disables its tick when idle
   (`lnomad/src/tui.rs:6157`). A messenger has relative timestamps, a sync
   schedule and retry deadlines. A one-second tick when there is anything
   pending, and a slower one otherwise, driven by `next_deadline()`
   (`leviculum-lxmf/src/router.rs:1827`).

Things to carry over unchanged: the generation counter for stale-result
rejection (`spawn_fetch`, `lnomad/src/tui.rs:5305-5346`), the tick-counted
toast whose expiry is a pure function and therefore unit-testable without
real time passing (`Toast`, `lnomad/src/tui.rs:661-676`, test at `:7271`),
the `TerminalGuard` RAII plus panic hook that restores the terminal before
the backtrace prints (`lnomad/src/tui.rs:5229-5273`), and OSC 52 for the
clipboard so copy works over SSH with no X11 dependency (`osc52`,
`lnomad/src/tui.rs:2519-2551`).

**One thing to fix from day one**: `lnomad` is 11,873 lines in
`src/tui.rs`. A messenger has strictly more state. Split
`model.rs` / `event.rs` / `update/` / `view/` / `shell.rs` before the first
thousand lines, not after the tenth.

## 6. Decision: scriptability

`lnomad`'s `--print` prints rendered ANSI and nothing machine-readable
(`lnomad/src/render.rs:143-146`); there is no JSON anywhere in the crate.
For a browser that is defensible. For a messenger it is a missed
opportunity: "send me a message when the backup finishes" is a real use and
needs no UI at all.

Non-interactive subcommands from the start, following `lnomad`'s automatic
non-tty detection (`lnomad/src/main.rs:167-168`) and its exit-code
convention (0, 1, 2):

```
lnmsg send <address> [--title T] [--from NAME] [--attach F] [--via direct|propagated] [-]
lnmsg read [--conversation A] [--since T] [--unread] [--json]
lnmsg sync [--json]
lnmsg contacts [--json]
lnmsg paper <address> -           # emit an lxm:// URI
lnmsg ingest <lxm://...>
```

with `--json` producing one object per line so `jq` works, and the exit
code distinguishing "sent" from "queued but not confirmed", which a script
genuinely needs to know.

**Decision (2026-08-10)**: `send` returns immediately with the message ID
on stdout; exit 0 means "queued cleanly" and claims nothing more, so it
never lies. `lnmsg status <id>` answers at any time (state, ledger,
`--json`). `--wait` opts into blocking until the delivery proof, with a
configurable timeout, and its exit codes distinguish delivered /
still-pending-at-timeout / terminally-failed — a timeout is not reported as
a failure, because the message may still arrive. Rationale: the common case
is a script that must not hang, and enqueueing is the only operation whose
success is knowable immediately; everything after it is a history, not a
result.

**Amended (2026-08-21): the message ID comes off stdout.** A successful
`lnmsg send` now prints nothing at all and exits 0; errors keep going to
stderr. The ID is not interesting to the person running the command, and
saying nothing on success is the ordinary Unix contract — a cron job that
mails its output should mail nothing when the send worked. The rest of this
decision is untouched: **exit 0 still means "queued cleanly" and claims
nothing about delivery**, and that is now the entire success signal, which is
why the exit code is what the tests assert. The ID does not become
unobtainable, because `lnmsg status <id>` needs it: `LNMSG_ENQUEUED … id=…`
carries it into the structured event log, which `LEVICULUM_EVENT_LOG=<path>`
turns on and which is written by an unfiltered layer, so the line arrives even
at the `warn` default (`leviculum-std/src/event_log.rs:513-520`). No
`--print-id` flag was added: nothing consumes the ID today, and an option
added against a hypothetical user is an option nobody tests.

**Decision (2026-08-21): the sender's name.** The delivery announce carried
the literal `lnmsg`, which names the tool rather than the person, so every
recipient saw the same sender for every operator on every host. The default
is now the account name, resolved in this order: `getpwuid(getuid())` first,
then `$USER`, then `$LOGNAME`, then `lnmsg` as a last resort. The password
database comes first deliberately — the first real consumer is a health
monitor started from cron, whose environment has no `$USER` at all, and a
name that is right interactively and wrong from cron would be discovered
late and by a machine. Resolution never fails: a status line that does not
go out is worse than one from an oddly-named sender.

`--from NAME`, and `LNMSG_DISPLAY_NAME` for the cron case, override it,
with the flag winning. They exist because a bare account name is ambiguous
when the same user runs the monitor on several machines — but what goes in
them is the operator's choice, not a policy of ours: no automatic hostname
suffix and no templating. An empty or whitespace-only override is exit 2,
not a silent fall back to the default, since it was set on purpose.
`lnmsg/src/display_name.rs` holds the order; `LNMSG_SENDER from=… source=…`
records which step answered, which is what separates a cron run that fell
through to the last resort from an interactive one that read `$USER`.

## 7. Structured event log

[Structured event logs](../structured-event-logs.md) and the project's
debugging discipline call for `EVENT_NAME key=val t=<ms>` lines. A
messenger that can be started with a log file, and whose every protocol
transition appears in it, is debuggable in the field in a way that no
terminal-scrollback client is. `lnomad` has no `tracing` dependency at all.
This one is cheap and is not treated as speculative.

## 8. What the library does not expose

Naming these is useful because each is a candidate issue.

**Decision (2026-08-10) on sequencing:** triage, not either extreme. Gaps
1, 2, 3 and 8 are closed in one library wave **before** `lnmsg` starts,
because the decided design cannot be built honestly without them: gap 2
blocks the mailbox glyph and the truthful delivery display outright, gap 1
blocks the naming-based trust model, gap 3 is shared infrastructure every
client rewrites, and gap 8 is small and stops two clients wording the same
errors differently. The remaining eight are filed as issues and met in
build order — the Codeberg #196 precedent (the library's biggest gap was
found by building a real consumer) argues for letting `lnmsg` discover the
gaps nobody has named yet, but waiting to "discover" a gap that is already
understood is delay, not empiricism. Gaps 10 and 11 are already covered by
the queued #204/#202/#203 batch; gap 7 shares its core-side prerequisite
with the S2 test-infrastructure question from the #212 work.

**Closed 2026-08-11.** The four are done: `RouterEvent::PeerAnnounced` (1),
`MessageState::AwaitingCollection` (2), `FileLxmfStorage` in `leviculum-std`
(3), and `Display` plus `core::error::Error` on the error types (8). The
entries below are left as written — they are the record of what was missing,
not a list of open work.

1. **No display name reaches the application.**
   `LxmfNodeEvent::PeerAnnounced` carries the destination hash only
   (`leviculum-lxmf/src/node.rs:133-135`), the router drops the name after
   reading the stamp cost (`leviculum-lxmf/src/router.rs:1173-1183`), and
   `RouterEvent` has no announce variant. Every client will re-implement
   announce filtering and `DeliveryAnnounce::decode`. A
   `RouterEvent::PeerAnnounced { destination, announce }` would remove that
   duplication.
2. **No "propagated, awaiting collection" state.** A propagated message
   reaches `Sent` and its queue entry is deleted
   (`leviculum-lxmf/src/router/propagation_runtime.rs:378-385`), so the
   client cannot distinguish "in a mailbox" from "gone" without keeping its
   own shadow record. This is the single biggest obstacle to an honest
   delivery display.
3. **No file-backed `LxmfStorage`.** Two implementations exist, both
   in-memory or null (`leviculum-lxmf/src/storage.rs:42`,
   `leviculum-lxmf/src/storage.rs:116`). Every host application writes the
   same one.
4. **No periodic sync scheduler and no interval config.**
   `PropagationClientConfig` has three fields
   (`leviculum-lxmf/src/router/propagation_runtime.rs:35-45`). Arguably
   correct for a sans-IO crate, but it means every client invents its own
   policy.
5. **Known propagation nodes and the selection are not in the snapshot**
   (`leviculum-lxmf/src/router.rs:1896-1913`), so every client writes its
   own persistence and replay.
6. **No stamp cancellation or deadline.** `generate` loops until success
   (`leviculum-lxmf/src/stamp.rs:171-182`) and `StampError::Cancelled` is
   declared but never constructed (`leviculum-lxmf/src/stamp.rs:24`). A
   user who starts a message to a high-cost peer and changes their mind has
   no way out.
7. **No inbound Resource cancellation**, stated as deliberate pending core
   support (`leviculum-lxmf/src/node.rs:429-430`). A user receiving a large
   attachment they do not want can only watch.
8. **Most error types are `Debug` only.** `RouterError`
   (`leviculum-lxmf/src/router.rs:340`), `LxmfNodeError`
   (`leviculum-lxmf/src/node.rs:233`), `PropagationTransportError`
   (`leviculum-lxmf/src/propagation_client.rs:144`), `MessageError`
   (`leviculum-lxmf/src/message.rs:40`) and `StorageError` have no
   `Display`. Every user-facing string is the client's to write, and two
   clients will word them differently.
9. **No typed codecs for reply, thread, reaction or renderer fields**
   (`leviculum-lxmf/src/constants.rs:28-46`), so each client hand-rolls
   msgpack for the same wire structures. This is a compatibility risk more
   than an ergonomics one.
10. **Codeberg #203** (`StampExecutor::generate` returns a `!Send` future)
    applies to us as it applied to `leviculum-lxmf-node`, which worked
    around it with a dedicated thread running a current-thread runtime
    (`leviculum-lxmf-node/src/main.rs:224-255`). We will make the same
    workaround.
11. **Codeberg #204** (a hook owns the events its own core calls return) is
    a documentation gap we will hit on day one. The bounded re-feed loop is
    not optional.
12. **Codeberg #186** (LXMF caches age on wall-clock time and are wiped by
    a timebase jump) matters more for a laptop that suspends than for a
    daemon that runs continuously, and should be checked against the
    suspend-resume path before it is dismissed.
