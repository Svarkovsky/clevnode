# lnmsg: conversation storage

Part of the [lnmsg design record](lnmsg.md).

The library stores one key (`ROUTER_STATE_KEY`, `b"lxmf/router-state"`,
`leviculum-lxmf/src/router.rs:53`) and hands each received message to the
application exactly once. **All history is the client's problem.**

## Options

**A. NomadNet's shape: a directory per conversation, a file per message.**
*For*: trivially crash-safe per message if each write is
temp-plus-rename; human-inspectable; deleting a conversation is `rm -rf`;
no dependency. *Against*: one inode per message forever; listing a
conversation is O(n) `listdir`; no search without reading everything;
NomadNet needed a `.index` sidecar that duplicates every message body on
disk (NomadNet's `Conversation.py`, lines 944-960) and still starves file
descriptors under announce storms (NomadNet commit `7bc6911`). On a
Raspberry Pi with an SD card this is the worst option.

**B. An append-only log per conversation plus a separate index.** Messages
appended as length-prefixed msgpack; an index mapping message ID to offset;
compaction on deletion. *For*: appends are one write and one fsync;
sequential reads are fast; the format is simple enough to recover by hand.
*Against*: you are writing a small database, including the index, the
compaction and the crash-consistency argument between them.

**C. SQLite.** One file, one `messages` table with the columns columba
already proved out. *For*: paging, search (FTS5), indices, transactions and
crash safety all solved by someone else; a 2 GB history is unremarkable;
`sqlite3` on the command line is the debugging tool. *Against*: `rusqlite`
with `bundled` compiles SQLite's C into the binary. `lnomad` treats a C
library in the path of the musl-static `.deb` as disqualifying
(`lnomad/Cargo.toml`, the `ratatui-image` comment), though that was about a
`pkg-config` probe for a *shared* library rather than a vendored static
one. **Checked empirically 2026-08-08**: `rusqlite` with `bundled` compiles
clean against `x86_64-unknown-linux-musl` on the project toolchain —
statically linked binary, SQLite 3.46 embedded, FTS5 verified working by
query, 2.6 MB total, 25 s build. The `lnomad` disqualifier was a
`pkg-config` probe for a shared library and does not apply to the vendored
static build.

**D. A pure-Rust embedded store (`redb`, `sled`, `fjall`).** *For*: no C
toolchain, ACID, ordered keys, so range scans give paging for free.
*Against*: no query language and no full-text search, so search is
hand-rolled; another dependency to trust with the user's mail.

## Decision (2026-08-10)

**C — SQLite**, via `rusqlite` with the `bundled` feature. The packaging
question was answered by the build probe above, so the fallback to D is
retired. The schema below, with its three commitments (identity-scoped
rows, two timestamps sorted on `COALESCE(received_at, timestamp)`, raw
msgpack `fields`), is the starting point; attachments live out of line as
content-addressed files. Retention is settled further down.

The schema to start from, taking columba's two good decisions:

```
messages(
  id BLOB, identity BLOB, conversation BLOB,
  direction INT, state INT, method INT, verification INT,
  timestamp REAL,        -- the sender's clock, from the wire
  received_at REAL,      -- our clock, when we saw it
  title BLOB, content BLOB, fields BLOB,   -- fields as raw msgpack
  error TEXT,
  PRIMARY KEY (id, identity))
```

with an index on `(conversation, identity, COALESCE(received_at, timestamp))`
and one on `(conversation, identity, direction, read)`.

Three reasons for the shape:

1. **Every row scoped by local identity.** A user may hold several
   addresses; columba's composite keys make that free, and retrofitting it
   later means a migration.
2. **Two timestamps, sort on `COALESCE(received_at, timestamp)`.** The wire
   timestamp is the sender's clock, and nothing makes another node's clock
   trustworthy. Sorting on it puts a peer with a wrong clock at the top or
   bottom of your history forever. NomadNet exposes this as a user-facing
   sort toggle, which is not a fix. (Sub-second precision is not the
   problem it once was on our side: the emission timestamp carries it, as
   the reference's `time.time()` does, because at whole-second granularity
   two identical messages created inside one second collapse to one ID —
   `leviculum-lxmf/src/router.rs:452-456`, Codeberg #217. Precision and
   skew are different failures, and only the second one is a sorting
   question.)
3. **`fields` stored as raw msgpack, not exploded into columns.** Unknown
   fields round-trip byte-for-byte in the library
   (`leviculum-lxmf/src/message.rs:5-8`) and must round-trip here too, or a
   reply to a message from a newer client loses information.

**Attachments out of line**, as NomadNet does (its `Conversation.py`, lines
752-812): content-addressed files under an `attachments/` directory, with
the row carrying names and hashes. Blobs in the database make the database
the size of the blobs, and a 2 MB voice message has no business in a row
you page through.

## Retention, decided 2026-08-10

Neither prior-art program prunes, and both will therefore eventually fail
on small hardware. The decision:

- **Message text is kept forever.** A million messages are a few hundred
  megabytes, SQLite territory, and the searchable archive is precisely the
  value the FTS decision bought.
- **Attachments live under a total-bytes budget** (default about 500 MB,
  configurable, numbers stated in the manual), evicting oldest first — the
  same pattern as `lnomad`'s byte-budgeted image cache
  (`lnomad/src/image_cache.rs:11-14`).
- **Evicting an attachment never touches the message row.** The message
  keeps the attachment's name, hash and size and renders "attachment
  (2.1 MB), evicted under the storage budget on <date>". The history does
  not lie, it just gets lighter.
- **Ledger rows** ([the delivery ledger](lnmsg-ui.md)) share the message's
  retention, so for text they live forever; no extra rule.
- A **per-conversation opt-in age cap** remains open as a possible later
  addition and was deliberately not built now.

## On the 2 GB Raspberry Pi question specifically

With option C, a 2 GB history is roughly ten million short messages, and
the operations that matter are "open the last screenful of a conversation"
and "search". Both are index lookups and neither touches the bulk. With
option A, opening a conversation reads every file in it. That asymmetry is
the whole argument.

## Durability rule, from `lnomad`'s counter-example

Nothing in this program may use `fs::write` on a file it cannot afford to
lose, and nothing may treat a corrupt load as an empty load
(`lnomad/src/bookmarks.rs:116-130`). Bookmarks can be silently forgotten.
Mail cannot.
