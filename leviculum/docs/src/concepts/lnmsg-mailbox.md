# lnmsg: the mailbox, and who you talk to

Part of the [lnmsg design record](lnmsg.md). Two decisions live here:
when and how the client talks to a propagation node, and how it names and
trusts the people on the other end. They belong together because the trust
model is what gates the mailbox choice.

## 1. Decision: propagation node interaction

This is where a naive design produces a client that silently loses mail,
and the library has arranged things so that the naive design is the
default: **nothing syncs unless the application asks**
(`request_messages_from_propagation_node`,
`leviculum-lxmf/src/router/propagation_runtime.rs:1323`, and
`next_deadline()` returns `None` outside `PathRequested`,
`leviculum-lxmf/src/router/propagation_runtime.rs:1134-1140`).

### When to sync

**Options**: on demand only; on a timer; on start plus timer; on announce
of the selected node; adaptive.

NomadNet uses a six-hour timer with a limit of eight messages and no sync
on start (NomadNet's `NomadNetworkApp.py`, lines 148-150 and 456-471). Six
hours is a very long time for something calling itself a messenger. columba
makes the interval configurable.

**Decision (2026-08-10)**: sync on start, on resume from suspend, on a
manual key, opportunistically when the selected node announces (free
evidence that it is reachable right now), and on a timer with **asymmetric
adaptivity**:

- The configured interval — default fifteen minutes — is a **hard upper
  bound that is never exceeded**. That is the promise the user can rely on:
  mail is at worst one interval old, always.
- After activity (a sync that returned something, or an outbound send), the
  client syncs more often for a while — on the order of every two minutes —
  decaying back to the bound. This is the chat-feel half of adaptivity
  ([the input-model decision](lnmsg-ui.md)), applied to the mailbox.
- The dangerous half — silently lengthening the interval when syncs come
  back empty — **does not exist**. Full adaptivity was rejected because an
  interval that quietly stretches is exactly the "why didn't I get that
  message for two hours" machine, and unpredictability in a messenger is a
  breach of trust.

Never sync while a direct link to the peer is up and working, because that
spends airtime to learn nothing.

The bound is configurable, its cost is stated in the manual in airtime
rather than in seconds (on LoRa a fifteen-minute poll is not cheap), and
the status line always shows when the next sync is due — the current
cadence is visible, never inferred.

### What the user sees

The library hands over a fourteen-state machine
(`PropagationClientState`,
`leviculum-lxmf/src/router/propagation_runtime.rs:60-75`), progress as an
`f32`, transfer size, and a result of `{ received, duplicates }`
(`PropagationSyncResult`,
`leviculum-lxmf/src/router/propagation_runtime.rs:78-83`). That is more
than enough to be honest.

A permanent one-line status, taking NomadNet's best idea (its
`Conversations.py`, lines 517-548) and refusing its modal dialog. Something
like:

```
mailbox  a1b2c3d4 Node-Name   3 hops   last sync 4m ago, 2 new   next in 11m
```

and during a sync the same line becomes the progress display, naming the
state in words: "asking the network where the node is", "connecting",
"asking what it has", "downloading 4 of 7". columba's plain-English state
descriptions are better than NomadNet's terse ones and both are better than
a bare progress bar.

**When there is no reachable node**, the line must say which of the several
different failures happened, because they need different fixes:

| Library state | What the user must be told |
|---|---|
| no node selected | "no mailbox chosen"; offer the picker |
| `NoPath` (`leviculum-lxmf/src/router/propagation_runtime.rs:69`) | "cannot find a route to the mailbox"; it may come back |
| `LinkFailed` (`leviculum-lxmf/src/router/propagation_runtime.rs:70`) | "the mailbox did not answer" |
| `NoAccess` (`leviculum-lxmf/src/router/propagation_runtime.rs:73`) | "the mailbox refused you"; this one will not fix itself |
| `NoIdentity` (`leviculum-lxmf/src/router/propagation_runtime.rs:72`) | "the mailbox does not know your key" |
| `TransferFailed` (`leviculum-lxmf/src/router/propagation_runtime.rs:71`) | "the transfer broke"; will retry |

A single "sync failed" for all six is the lie this section exists to
prevent.

### Which node, and the trust question

NomadNet auto-selects the fewest-hops node **whose trust level is
TRUSTED** (its `NomadNetworkApp.py`, lines 607-631). columba auto-selects
the fewest-hops node, full stop. The library's own auto-selection ranks by
route, hops, peering cost and stamp cost
(`select_outbound_propagation_node`,
`leviculum-lxmf/src/router/propagation_runtime.rs:1153-1188`) with no trust
input at all, because it has no notion of trust.

Your mailbox sees the *envelope* of every message sent to you: who sent it
and when, even though it cannot read the content. Handing that to whoever
happens to be nearest is a real privacy decision, and neither prior-art
program presents it as one.

**Never auto-adopt silently.** On first run, and whenever the selected node
becomes unreachable, present a picker with the candidates, their hop
counts, their advertised limits and costs (`PropagationNodeAnnounce`,
`leviculum-lxmf/src/propagation.rs:513-525`), and require one keystroke to
accept. Automatic *failover* between nodes the user has already approved is
fine and the library already does it
(`leviculum-lxmf/src/router/propagation_runtime.rs:817-838`); automatic
*adoption* of a stranger is not.

Note that NomadNet's trust propagation makes this worse: trusting a person
auto-trusts their node (its `Directory.py`, lines 198-202), which makes it
eligible as your mailbox. Do not inherit that.

### The purge default

`retain_synced_on_node` defaults to `false`
(`leviculum-lxmf/src/router/propagation_runtime.rs:50`), so by default the
client tells the node to delete what it has collected. That is the right
default for privacy and for the node operator's disk, and it is the wrong
default for a user who runs two clients on the same identity, because the
first one to sync takes the mail. This must be a visible setting with the
consequence spelled out, not a config-file default nobody reads.

### What the client must implement itself

- The sync schedule (there is none in the library).
- Persistence of known propagation nodes and of the selection, since
  neither is in the router snapshot (`snapshot`,
  `leviculum-lxmf/src/router.rs:1896-1913`); replay via
  `restore_known_propagation_node`
  (`leviculum-lxmf/src/router/propagation_runtime.rs:1309`).
- Re-selection after restart.
- Proof-of-work for `PropagationStampPending`, off the core lock.
- Calling `persist()` on `PersistenceRequested`.

### The mailbox as a visible relationship (proposal)

The propagation node is currently magic in every client: something chosen
for you, syncing on a schedule you did not set, holding mail you cannot
see. Make it a first-class object in the UI, with its own screen: who it
is, how many hops away, what it advertises, when you last spoke to it, what
it is holding for you if it will say, and whether it is purging what you
collect. columba's trick of modelling the relay as a contact with an
`isMyRelay` flag is a cheap way to get there.

The honest version of this includes telling the user what the mailbox
learns about them: the envelope of every message they receive. No prior
program says this out loud.

## 2. Decision: identity, contacts, and names

The address is the identity. A 16-byte destination hash, rendered as 32 hex
characters, is the only thing that is true about a peer.

The delivery announce carries a display name as arbitrary bytes
(`DeliveryAnnounce`, `leviculum-lxmf/src/announce.rs:39-44`), and
`display_name()` strips NUL and trims but does nothing else
(`leviculum-lxmf/src/announce.rs:164-168`). The announce *is* signed: the
Reticulum announce signature covers the app data
(`leviculum-core/src/announce.rs:104`, `:213`: "the signature covers
`destination_hash + public_key + name_hash + random_hash + [ratchet] +
app_data`"). So a verified announce proves that the holder of that key
chose that name. It proves nothing about uniqueness, and there is no naming
authority in Reticulum. Two identities can both announce "Lew", and one of
them can be doing it on purpose.

Two further facts a UI must not paper over. `Message::verification` can be
`Unverified` when the source identity has never been announced to us
(`leviculum-lxmf/src/message.rs:213-215`), and such messages **are
delivered to the application anyway**
(`leviculum-lxmf/src/router.rs:1414-1417`). And the router discards the
display name from announces entirely, so **the client must maintain its own
hash-to-name map** from raw `NodeEvent::AnnounceReceived`.

### Options for naming

**A. Announced name only.** What most chat UIs do. Simple, and
impersonation is trivial.

**B. Local petname only.** Nothing is displayed until the user names the
contact; strangers show as a hash prefix. Maximally safe, maximally
tedious, and hostile to the case where someone new writes to you.

**C. Petname wins, announced name shown as provenance.** columba's
precedence chain (nickname, then announced, then stored), with the
announced name still visible somewhere.

### Decision

**C, plus NomadNet's collision check, plus a hash that never fully
disappears.**

1. **Display**: petname if set, otherwise the announced name, and in both
   cases a short hash suffix. NomadNet suppresses the hash for trusted
   peers (its `Directory.py`, lines 277-297); we shorten it rather than
   suppress it, because a four-character hash costs nothing and makes
   "wait, that is not the Lew I know" possible at a glance.
2. **Collision warning**, the one genuinely novel thing in the prior art:
   if an announced name is already claimed by a different address, mark it
   (NomadNet's `Directory.py`, lines 306-320). Extend it to a
   **name-change warning**: if an address you have talked to announces a
   different name than last time, say so once in the conversation. That is
   cheap, and it is the actual impersonation vector.
3. **Sanitise names.** They are arbitrary bytes off the wire. Strip control
   characters, normalise, cap the display width, and refuse to let a name
   contain something that renders as a checkmark or as another contact's
   name. NomadNet does this because micron markup in a name would otherwise
   render (its `util.py`, `strip_modifiers`).
4. **Unverified messages must look different.** NomadNet's "Unknown Origin"
   / "Invalid Signature" plain-English rendering (its `Conversation.py`,
   lines 620-630) is right; a coloured glyph alone is not.
5. **Refuse to compose to an address whose key is unknown**, with the
   reason and a "ask the network" action, as NomadNet does (its
   `Conversations.py`, lines 2186-2204). The library will tell you: our own
   helper checks `core.storage().get_identity(&peer)` before composing and
   reports which call was skipped rather than timing out later
   (`leviculum-lxmf-node/src/processor.rs:683-692`).
6. **Contact status as persisted state**, columba's `ACTIVE` /
   `PENDING_IDENTITY` / `UNRESOLVED`, rather than a live lookup, so the
   list can be rendered without touching the network.

### Trust levels: how many?

NomadNet has four — `WARNING`, `UNTRUSTED`, `UNKNOWN`, `TRUSTED` (its
`Directory.py`, lines 410-413); columba has none. Four levels with a radio
group is more ceremony than most users will perform.

**Decision (2026-08-10)**: exactly three states that a user reaches by
accident and understands: **known** (I named this contact), **unknown** (I
have not), and **blocked**. Trust is not a thing to configure but a thing
that follows from having named someone, which is an action people take
anyway.

A derived fourth state ("suspicious", computed from name collisions and
name changes) was considered and **rejected**: on a mesh, two identities
sharing a display name and a contact renaming themselves are normal
occurrences, not indicators of attack, and a state that cries wolf on
normal behaviour trains the user to ignore it. The signals themselves are
not discarded — they are a *rendering* matter, not a trust matter: when two
contacts share a display name the list must disambiguate them (short hash
suffix), and identity, not name, is always what messages are keyed by. The
columba defect this section opened with was the missing trust anchor for
relay auto-selection, and three states cover that: only **known** contacts
qualify.

The identity file itself must not behave like `lnomad`'s
(`load_or_create`, `lnomad/src/identity.rs:39-53`, silently regenerating on
a decode failure). A corrupt identity is a refusal to start with a clear
message, because minting a new one silently changes the user's address.
