# lnmsg: the user interface

Part of the [lnmsg design record](lnmsg.md). Two decisions live here: how
keys are dispatched, and what an honest delivery display looks like. They
share one discipline — a single table read by everything that renders from
it — and that discipline is the reason both are on one page.

## 1. Decision: the input model

This is the hard one, and the browser's answer does not transfer. In a
browser, plain letters are free because there is nothing to type into most
of the time. In a chat client, the single most common action is typing
prose.

### Options

**A. Modal, vi-style.** Normal mode for commands, insert mode for
composing, `i` to enter, `Esc` to leave.
*For*: every key stays available for commands; scales to any number of
bindings; vi users are instantly at home; it is the only option where
`j`/`k` mean what they mean in `lnomad`.
*Against*: it is the single biggest complaint non-vi users have about
terminal software. A user who types a message, presses `Esc` out of habit,
and then types "quit" has issued four commands. Mode errors in a messenger
are worse than in an editor because the consequence can be sending
something.

**B. Always-insert, everything Ctrl-modified.** NomadNet's answer
(NomadNet's `Conversations.py`, lines 68-80). Focus starts in the compose
box, plain typing always composes, every command is `Ctrl-`something.
*For*: zero mode errors; a user who has never read the manual can still
type and send. *Against*: the Ctrl namespace is about twenty-six slots wide
and readline already claims a dozen of them. NomadNet ran out and started
overloading by focus, so `Ctrl-X` means two different things depending on
invisible state.

**C. Focus-follows-pane.** The conversation list and the message list are
command panes with `lnomad`-style bindings; the compose box is a text pane
where plain keys type. `Tab` moves focus.
*For*: no modes to learn, because the mode is visible as which pane has the
cursor; single-letter commands survive in the panes where they make sense;
it matches how tmux, mutt and every mail client behave.
*Against*: the same key does different things in different panes, which is
a mode by another name, just one you can see. And `Tab` becomes precious.

**D. Prefix key.** Everything types; a prefix (`Ctrl-Space`, `Ctrl-A`, or
`,`) introduces a command.
*For*: unlimited namespace, no modes, tmux and screen users know it.
*Against*: two keystrokes for everything, including scrolling, which is the
operation you do most.

### Decision (2026-08-08)

**C, with a command palette (D) layered on it, and no modes.**

Concretely:

- Three panes: conversations (left), messages (right, upper), compose
  (right, lower). `Tab` and `Shift-Tab` cycle; a click focuses.
- **In the conversation and message panes**, `lnomad`'s keymap applies
  unchanged: the `SCROLL_KEYS` table verbatim
  (`lnomad/src/tui.rs:3159-3291`), so `j`/`k`, `Ctrl-n`/`Ctrl-p`, arrows,
  `Ctrl-f`/`Ctrl-b`, `Ctrl-v`/`Alt-v`, `Ctrl-d`/`Ctrl-u`, `g`/`G`,
  `Home`/`End` and the wheel all work, in all four idioms, for free.
- **In the compose pane**, plain keys type. Only `Ctrl-` and `Alt-` chords
  are commands, plus the scroll table's `Ctrl` and `Alt` rows, which do not
  collide with readline because they are page motions and readline's are
  line motions. **`Enter` sends**: LXMF is email on the wire but chat in
  every deployed client — Sideband, NomadNet, MeshChat and columba all
  render conversations, nobody uses the title field — so users arrive with
  chat expectations, and Enter-to-send is the universal chat convention.
  Newline is `Alt-Enter` everywhere and additionally `Shift-Enter` where
  the terminal speaks the keyboard enhancement protocol (legacy terminals
  cannot distinguish `Shift-Enter` or `Ctrl-Enter` from `Enter` — same byte
  — which is also why `Ctrl-Enter`-to-send was dropped; crossterm's
  `PushKeyboardEnhancementFlags` enables the modern protocol where
  available). `Enter` on an empty buffer does nothing. The keymap table
  carries a `send_on_enter` switch; off flips to email style (`Enter`
  newline, `Ctrl-D` send) for long-form writers. `Ctrl-D` on an empty
  buffer must not send and must not quit.
- **A command palette** on `:` in a command pane and `Ctrl-P` in the
  compose pane, with fuzzy matching over named commands. This is what makes
  the scheme learnable: every command has a name, the palette lists them
  all, and each entry shows its key if it has one. NomadNet's static
  shortcut bar is the failure mode to avoid.
- **`f` hint mode from `lnomad`** (`hints`,
  `lnomad/src/tui.rs:1155-1195`), extended to conversations. Its best
  property is that it matches either the hint label *or* a substring of the
  target's text (`hint_matches`, `lnomad/src/tui.rs:1182-1195`), so `f`
  then typing part of a contact's name jumps to that conversation. That is
  a better contact switcher than anything in either prior-art program.

**What must not be copied**: `lnomad` quits on `Ctrl-C` from any mode,
before the mode dispatch (`lnomad/src/tui.rs:1575-1578`). In a messenger
that is a half-written message thrown away by a reflex. `Ctrl-C` in the
compose pane clears the buffer to a recoverable draft; quitting needs
`Ctrl-Q` or the palette.

**The keymap must be one table.** `lnomad` applied single-source-of-truth
discipline to scrolling and nowhere else, and its help overlay's other
groups are hand-typed strings that can drift
(`lnomad/src/tui.rs:4687-4788`). Every binding here is one table read by
the key handler, the help overlay, the palette and the footer hints. That
table is also the natural place to hang user configuration later, which
`lnomad` has none of.

**What would change this**: if the compose box turns out to be where users
spend nearly all their time, B (always-insert, Ctrl for everything) is
simpler and has no invisible state at all, at the cost of losing
single-letter commands entirely. The way to find out is to build C and
count how often focus is in the compose pane.

**The quiet keyboard, with its one exception** (decided 2026-08-08). The
`lnomad` rule that single letters are reserved for cheap local actions
(`lnomad/src/tui.rs:1930-1933`) generalises here to: **no single keystroke
may put bytes on the air — except `Enter` in the compose pane.** The
exception is principled, not a leak: the deliberate act was typing the
message into a deliberately focused pane, and `Enter` completes that act.
Everywhere else the rule is absolute: no key in a command pane transmits,
syncing needs a chord or the palette, announcing needs a chord or the
palette. Everything free is free.

## 2. Decision: what honesty about delivery looks like

Most chat UIs lie by simplification: one check for sent, two for delivered,
and everything ambiguous rounded to the friendlier reading. LXMF has real
states and the library reports them, so there is no excuse.

The states as they actually are, from
[the library survey](lnmsg-architecture.md):

| Situation | Library state | What is actually true |
|---|---|---|
| queued, waiting for a route or a retry | `Outbound` | nothing has been transmitted |
| computing proof-of-work | `Outbound` + `StampPending` | nothing has been transmitted, and it may take a while |
| link being built, or bytes going out | `Sending` | in flight, no confirmation |
| opportunistic packet handed to Reticulum | `Sent` | transmitted once, unproven, still retryable |
| propagation node accepted the upload | `Sent`, entry deleted | it is in a mailbox; the recipient may never collect it |
| transport proof received | `Delivered` | the bytes reached the destination identity |
| receiver cancelled the resource | `Rejected` | they refused it, or their client did |
| node refused the upload (bad stamp) | `Rejected` | the mailbox refused it, not the recipient |
| five attempts exhausted | `Failed` | give up, keep the text |

### What a truthful UI shows

**Different marks for different truths, and words in the detail view.** The
three that must never be collapsed:

- **handed to the network, unproven** (opportunistic `Sent`),
- **left in a mailbox for later** (propagated `Sent`),
- **arrived at the destination** (`Delivered`).

columba collapses the first two into one check mark; NomadNet gives them
different colours but never words. Three visually distinct marks, and,
crucially, **a per-message detail view** (columba's best idea) showing
delivery method with a one-sentence explanation, attempt count, hop count,
and the error string when there is one. In a terminal that is a key press
on a focused message.

**Never claim a read receipt.** LXMF has no such field
(`leviculum-lxmf/src/constants.rs:20-46`). Any UI element that suggests one
is a lie in the protocol's own terms.

**Say what `Delivered` means, once.** It means the bytes reached the
destination identity, not that a human saw them. The detail view is where
that sentence lives.

**Distinguish the two `Rejected`s** by correlating with the message's
method before the entry disappears. "Your mailbox refused this message" and
"the recipient's client refused this message" are different problems with
different fixes.

**Handle the restart discontinuity honestly.** `restore` resets every
queued message to `Outbound` (`leviculum-lxmf/src/router.rs:1881-1884`). So
after a restart, a message that was "sending" is "queued" again, and the UI
must show that rather than a frozen progress bar. NomadNet's equivalent,
forcing a stale mid-flight message to `FAILED` on load (NomadNet's
`Conversation.py`, lines 455-467), is at least honest, though our library
gives us the better option of an honest retry.

**Never let a late failure demote a success.** columba added exactly this
guard after a real bug. `Delivered` must be terminal in the UI even if
something arrives afterwards claiming otherwise.

### Decision (2026-08-10): the ledger and the glyph

**The ledger is one keypress away, inside the per-message detail view** —
part of the normal program, not a debug flag, but costing no space in the
conversation. It is also where the airtime number lives. The ledger rows
are stored beside the message in SQLite (a handful of roughly 50-byte rows
per message) and share the message's retention
([storage](lnmsg-storage.md)).

**Decided with it: one coloured glyph per message in the conversation
view**, occupying a single cell, carrying the delivery state at a glance.
The glyph is the compact face of the same state machine the detail view
explains; both render from **one shared state-mapping table** (the keymap
discipline above, applied again — the glyph, the detail view and the ledger
can never disagree).

The glyph language, chosen so the colours carry the meaning even before the
shapes are learned:

| state | glyph | colour | motion |
|---|---|---|---|
| queued, nothing transmitted | `○` | dim gray | static |
| computing proof-of-work | braille spinner `⠋⠙⠸⠴⠦⠇` | dim yellow | animated on the tick |
| in flight (link building, bytes out) | braille spinner | yellow | animated on the tick |
| handed to the network, unproven (opportunistic `Sent`) | `◇` | amber | static |
| left in a mailbox (propagated `Sent`) | `⌂` | blue | static |
| arrived (`Delivered`) | `✓` | green | static, terminal |
| refused (`Rejected`, either kind) | `✗` | red | static |
| given up (`Failed`) | `✗` | dim red | static |

The rules the table encodes: **green and a check mark appear only on
proof** — the mailbox state is a blue house, deliberately not a second
check, because "in a mailbox the recipient may never open" must not read as
progress toward delivered; amber `◇` (hollow) against green `✓` (solid)
mirrors unproven-versus-proven; animation means "the machine is working
right now" and nothing else, driven by the
[event-loop tick](lnmsg-architecture.md) so it freezes honestly if the
program hangs. An ASCII fallback set (`. * o ^ v x`) ships behind the same
table for terminals that mangle the glyphs.

The timeline itself:

```
14:02:11  queued
14:02:11  no route known, asked the network
14:02:19  route found, 3 hops
14:02:20  link established
14:02:21  sent, 412 bytes
14:02:26  delivery proof received
```

This is close to free, since the library already emits every one of those
transitions, and it turns "why is this taking so long" from a support
question into something the user can read.

## 3. Proposals, not requirements

Everything in this section is a proposal rather than a decision, and
several of these will not survive contact with a real user.

### Cost before commitment

Show what a message will cost before it is sent, next to the send action:

```
412 bytes   ~7 s airtime at the slowest hop   no stamp required
```

The pieces exist. `leviculum_core::rnode::airtime_ms`
(`leviculum-core/src/rnode.rs:894`) and `packet_airtime_ms`
(`leviculum-core/src/rnode.rs:1337`) are public, interfaces report a
`bitrate` (`leviculum-std/src/interfaces/mod.rs:464-466`) computed from
spreading factor, coding rate and bandwidth
(`compute_bitrate`, `leviculum-std/src/interfaces/rnode.rs:1411`), and
`fetch_remote_status` (`leviculum-std/src/remote_status.rs:184`) retrieves
the interface list from the daemon, which is how `lnstatus` works. Note two
honesty constraints: `fetch_remote_status` needs the management authkey,
and the status surface reports `bitrate` but not the raw radio parameters,
so an estimate from `bytes * 8 / bitrate` is the best available and must be
labelled as an estimate.

When the peer advertises a stamp cost, the estimate must include the
proof-of-work, and that number has to be *measured* first: the crate
contains no benchmarks, and the cost model (2^cost hashes plus a 3000-round
workblock, `leviculum-lxmf/src/constants.rs:16`,
`leviculum-lxmf/src/stamp.rs:258-266`) predicts scaling but not
milliseconds on a Pi.

### Offline as a state, not a failure

An offline-first client should look deliberate rather than broken. Two
concrete moves:

- A single **posture line** that says what the program can do right now:
  "3 peers reachable directly, mailbox 4m ago, 2 messages waiting to send".
  Not a red error banner; a statement of fact.
- **Queued messages shown in the conversation**, in place, greyed, with
  their reason ("waiting for a route", "computing proof-of-work, about a
  minute"). A message the user wrote should never vanish into a queue they
  cannot see. The library gives progress and next-attempt time per entry
  (`OutboundEntry`, `leviculum-lxmf/src/router/outbound.rs:58-72`).

### The mouse as a first-class citizen

`lnomad` enables mouse capture unconditionally with no toggle
(`lnomad/src/tui.rs:5215-5218`) and handles neither drag nor selection
(`lnomad/src/tui.rs:1374`). In a browser that costs you the terminal's own
copy-paste; in a messenger, where copying message text is a constant, it is
worse.

Proposal: handle drag selection ourselves over the message IR, so selecting
text across wrapped lines and across message boundaries works and copies
via OSC 52 (`lnomad/src/tui.rs:2519-2551`, which works over SSH). Plus a
`--no-mouse` flag and a runtime toggle for people who want the terminal's
own selection back. The `StyledChar` IR already carries per-cell ownership
(`lnomad/src/render.rs:340-354`), so the hit-testing is a small extension
of `visible_links` rather than new machinery.

### Terminal QR for paper messages and for your own address

`PaperMessage::to_uri()` produces an `lxm://` URI
(`leviculum-lxmf/src/paper.rs:170`) and the crate stops there. A QR code
rendered in Unicode half blocks is a well-trodden trick, and `lnomad`
already has the half-block ladder for images. That gives an air-gapped send
path: compose, render, photograph, and the recipient scans it. Also useful
for showing your own address to someone sitting next to you, which NomadNet
does (`Ctrl-P` in the conversation list).

Constraint: `PAPER_MDU` is 2210 bytes
(`leviculum-lxmf/src/constants.rs:9`), which is near the practical limit of
what a QR code can hold and certainly beyond what a phone camera reads off
a terminal at normal font sizes. The UI must say when a message is too big
to be a QR and offer the URI as text instead.

### A "what changed while I was away" view

On start, after the first sync, one screen summarising what arrived,
grouped by conversation, with the option to mark all read or step through
them. Every mail client has this; no Reticulum client does. It fits the
usage pattern exactly, because the whole point of the propagation node is
that the user was away.

### Micron in messages

If `FIELD_RENDERER` says micron (`reference/LXMF/LXMF/LXMF.py:100`), render
it with `leviculum-micron` (`leviculum-micron/src/lib.rs:25-27`) and
`lnomad`'s renderer. If it says markdown, we already depend on
`pulldown-cmark` at workspace level (`Cargo.toml:82`). This is a capability
the workspace has and nobody has spent, and it costs a match statement.

Sending micron is the more interesting half: a compose box with a preview
toggle, in a client whose sister program is a micron browser.
