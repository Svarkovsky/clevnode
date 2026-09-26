# Public channels over LXMF

**Status: design record. Not yet implemented.**

Meshtastic and MeshCore both have public channels, and they are the reason
a lot of people pick those stacks. You flash a board, type a channel name,
and you are talking to whoever is in range. LXMF has nothing equivalent.
It has mailboxes, and a mailbox is a conversation with one person.

This document describes how to add channels without changing anything in
the existing Reticulum or LXMF infrastructure, why that is possible at
all, and where the sharp edges are.

Two crates are involved, neither of which exists yet: a propagation node
server, and channel support in `lnmsg`. The lnmsg design record currently
states that hosting a propagation node is out of scope. That changes here:
the channel feature only works if we host nodes, because the retrieval
side is ours.


## 1. Why the obvious approach does not work

Reticulum has a broadcast packet form and a symmetric group destination
type, and neither carries a channel.

`Destination.announce()` refuses anything that is not a SINGLE destination
(`Destination.py:251-252`), so a GROUP or PLAIN destination can never be
announced, and without an announce there is no path table entry.
`Transport.outbound` skips the path lookup for both types outright
(`Transport.py:1121`), and any receiving node drops such a packet once
`hops > 1` (`Transport.py:1354-1373`, mirrored in
`leviculum-core/src/transport.rs:2196-2228`). The reach of a Reticulum
broadcast is exactly one hop plus locally attached clients.

For PLAIN the manual gives the reason: "To be transportable over multiple
hops in Reticulum, information *must* be encrypted, since Reticulum uses
the per-packet encryption to verify routing paths and keep them alive"
(`docs/source/understanding.rst:112-114`). For GROUP the same manual says
only that packets are "not *currently*" carried over multiple hops,
"although a planned upgrade to Reticulum will allow globally reachable
*group* destinations" (`understanding.rst:118-120`). That sentence has
stood unchanged since 2022-04-28.

The other flooding primitive, the announce, is capped at two percent of
interface bandwidth by `ANNOUNCE_CAP` (`Reticulum.py:114`), with rate
penalties on top. It is not a carrier for chat.


## 2. The carrier that does exist

Propagation nodes flood among themselves over the full multi-hop
transport, because peer-to-peer sync runs over ordinary links between
SINGLE destinations. An earlier draft of this document called that
flooding "blind", and overstated it: a node never looks *inside* what it
stores, but it does not accept unconditionally. Three properties make the
carrier usable, and two toll gates stand in front of it.

**A node does not inspect what it stores.** `lxmf_propagation`
(`LXMRouter.py:2487-2518`) requires only that the data is at least
`LXMF_OVERHEAD` bytes long (`LXMessage.py:63`) and that its transient ID,
the SHA-256 of the bytes, is new. It then takes the first 16 bytes as the
destination hash, writes the message to disk and queues it for
distribution. There is no signature check, no decryption, and no check
that the destination has ever been announced or exists.

**Distribution is unfiltered.** `flush_peer_distribution_queue`
(`LXMRouter.py:2472-2485`) offers every new message to every peer except
the one it came from, with no criterion whatsoever.

**Peering is automatic — within three limits.** A node peers with any
node whose announce it hears, as long as `autopeer` is set and the hop
distance is within `autopeer_maxdepth` (`Handlers.py:81-83`, and again on
inbound sync at `LXMRouter.py:2365`). Three gates bound it. A peer whose
announced peering cost exceeds the local `max_peering_cost` is refused
(`LXMRouter.py:2005-2010`; default maximum 26, `LXMRouter.py:50-51`). A
node that already holds `MAX_PEERS` peers — twenty by default
(`LXMRouter.py:43`) — refuses further ones (`LXMRouter.py:2032`). And an
inbound sync offer must present a peering key: a proof of work over the
two node identities at the announced peering cost, 18 bits by default,
checked by `validate_peering_key` (`LXMRouter.py:2300-2312`). An offer
without a valid key is rejected with `ERROR_INVALID_KEY`, and a party
without one may deliver at most one message per transfer
(`LXMRouter.py:2382-2385`).

**Every stored message has paid for admission.** Both ingest paths — the
link-packet path and the resource path — run `validate_pn_stamps`
(`LXMRouter.py:2242-2243`, again at `LXMRouter.py:2401-2402`) before
`lxmf_propagation` ever sees a byte. A propagation stamp is a proof of
work over the message, carried as 32 trailing bytes. The minimum accepted
cost is the node's `propagation_stamp_cost` minus its flexibility — 16
minus 3 with the defaults (`LXMRouter.py:52-54`), and the cost knob is
clamped so it can never be configured below `PROPAGATION_COST_MIN` = 13
(`LXMRouter.py:137`). An unstamped or understamped message is dropped; a
transfer containing one tears the link down and throttles the sender for
`PN_STAMP_THROTTLE` = 180 seconds (`LXMRouter.py:63`, applied at
`LXMRouter.py:2449-2452`).

So a channel message injected anywhere reaches every propagation node in
the connected network — *provided* it carries a stamp the ingest node
accepts, which with default configurations means minting 16 bits of work
per post, and never less than 13. Existing Python nodes are the
transport; our nodes are the access points. Nothing upstream has to
change for the distribution half to work, but a node of ours that wants
to distribute has to implement all of the above: the announce format
position by position, peering-key minting and validation, stamp minting
and validation, transfer and sync limits, and the throttle behaviour. A
node built without the peering key gets `ERROR_INVALID_KEY` on every sync
offer it makes and is limited to one message per transfer; a node that
skips stamp validation accepts what its peers will not. Either way the
distribution half collapses.

Writing does not even require one of our nodes. A client with no
presented identity may deliver one stamped message per transfer to any
node, and that message is flooded normally. Only reading requires us.


## 3. What we must not build

The retrieval endpoint has to work without an identity check, or channels
do not work at all: many people read the same address, and none of them
owns it. The naive form is "client asks for a destination hash, node
returns what it holds".

That must not be built. Through peer sync our node also stores every
foreign mailbox message in the network. An endpoint that answers for an
arbitrary destination hash becomes a traffic-analysis oracle over the
whole mesh: ask about any address and learn how many messages are
pending, how large they are, and when they arrived. The contents stay
encrypted, and it still destroys exactly the property Reticulum exists to
provide.

**The rule: a channel endpoint may only ever serve entries the node has
positively classified as channel entries.** Mailbox entries are reachable
solely through the identity-bound `/get` path, unchanged from Python:
`message_get_request` (`LXMRouter.py:1482-1484`) refuses a client that
presents no identity, and serves only entries addressed to the
destination derived from that identity. Section 5 is what makes the
classification sound rather than a matter of trust.

What the rule does not give: any operator of any propagation node —
Python or ours — sees the destination hash of every stored entry in
plaintext, and can watch per-address volume and timing locally. The rule
closes the *remote* oracle, the one that answers strangers about
addresses they merely name. It cannot close the local view, because
carrying traffic means seeing it. Section 4 states what that costs each
channel type.


## 4. Addressing

Both channel types are ordinary 16-byte destination hashes, derived with
the standard Reticulum construction, `Destination.hash`
(`Destination.py:116-130`), so that stock tooling can compute them
unmodified.

**Open channels** derive from the name alone:

    name_hash = SHA-256("lxmf.channel." + name)[:10]
    dest_hash = SHA-256(name_hash)[:16]

which is `Destination.hash(None, "lxmf", "channel", name)`. Anyone who
knows the name can compute the address, which is the point.

A key-stretching alternative was considered and rejected: derive the
address through an expensive KDF over the name instead of one cheap hash,
so that an observer holding a stored entry's address cannot cheaply
dictionary-test candidate names against it. It buys nothing here. Every
open-channel message carries its name in cleartext (section 5), because
the node must recompute the address from the name to classify the entry
at all — so the name is public the moment the first post exists, and
hardening the derivation only slows down legitimate clients on weak
hardware. For closed groups the question does not arise, because their
addresses do not derive from names.

**Closed groups.** This section replaces an earlier design that review
killed, and the replacement is a **proposal, not a settled decision** —
it changes the shape of the group secret and needs its own review before
anything is built.

> The earlier draft derived the group address from a symmetric group key
> and authenticated readers by an HMAC under a second symmetric key. One
> sentence on why that was unsound, so the scar stays visible: the node
> never holds any group's key, so it could verify neither the HMAC nor
> the claimed binding between address and key — the "authentication"
> reduced to knowledge of a 16-byte address, which is exactly the oracle
> section 3 forbids.

The proposal: a group is an ordinary Reticulum identity — an asymmetric
keypair — whose full key material is shared among the members as the
group secret, alongside a symmetric content key for the payloads. The
address derives from the group identity with the standard construction
and a *fixed* aspect set:

    dest_hash = Destination.hash(group_identity, "lxmf", "group")

`Destination.hash` accepts an identity (`Destination.py:122-124`) or 16
bytes of raw hash material (`Destination.py:125-126`); the proposal uses
the identity form. No name enters the derivation, so a node holding the
group's *public* key can recompute the address — and verify that key and
address belong together — without ever learning a name or a secret. That
recomputation is what makes closed-group authentication and
classification verifiable (sections 5 and 7) where the HMAC design was
not.

What a closed group hides, stated honestly rather than generously: the
name and the contents. Not the existence, and not the traffic. The
address stands in plaintext as the first 16 bytes of every stored entry
on every node that syncs it, and the envelope that makes classification
possible (section 5) marks the entry as closed-group traffic and carries
the group's public key. After the first post, any node operator in the
network can observe that this group exists, how much it posts and when.
Without the group secret the address cannot be *derived in advance* and
the group cannot be found by name — but "unguessable and undiscoverable",
as the earlier draft had it, overstated the property. Members who need
their group's existence hidden from node operators need a different tool.

Both channel namespaces are disjoint from `lxmf.delivery` by the name
hash that enters the construction, so a channel or group hash can never
collide with a mailbox hash short of a SHA-256 collision.


## 5. Message format, and how a node recognises a channel message

A channel message is an ordinary LXMF message whose destination hash is
the channel address. The packed form is `destination(16) || source(16) ||
signature(64) || payload`, where the payload is a msgpack array
(`LXMessage.py:382-386`). The signature covers destination, source,
payload and the message hash (`LXMessage.py:364-368` builds the hashed
part, `LXMessage.py:375-378` signs it).

For propagation, Python encrypts everything after the destination hash
and appends the propagation stamp: `destination || encrypt(rest) ||
stamp` (`LXMessage.py:430-435`). An open channel message differs in
exactly one step: the rest is *not* encrypted, so the stored body is
readable. A receiving Python node cannot tell the difference, because it
never decrypts either one. On disk, a propagation node stores the entry
with the validated stamp re-appended (`LXMRouter.py:2512-2515`) and
strips it again when serving (`LXMRouter.py:1549`); the stamp is 32 bytes
(`STAMP_SIZE`, `LXStamper.py:15`).

Three reserved custom fields carry the channel data (`LXMF.py:44-46`):

- `FIELD_CUSTOM_TYPE (0xFB)`: the discriminator and format version.
- `FIELD_CUSTOM_META (0xFD)`: the channel name, and the sender's public
  key.
- `FIELD_CUSTOM_DATA (0xFC)`: reserved for later use.

**Classification of open-channel entries.** The classifier takes a stored
entry, strips the trailing 32-byte stamp, and parses the region after
byte 96 — after destination, source and signature, not after the
destination alone — as msgpack. An earlier draft parsed at the wrong
offset and claimed that encrypted mailbox traffic "fails this
immediately, being ciphertext". Both halves were wrong: a literal
implementation would have failed on every legitimate entry, and
ciphertext does not reliably fail a msgpack parse — any byte in
`0x00-0x7f` is a complete, valid positive-fixint document, and larger
accidental structures parse too. Parsing is a cheap prefilter, nothing
more. The checks, in order:

1. The entry is at least `LXMF_OVERHEAD` plus stamp bytes long.
2. The region after byte 96 parses as a msgpack array of four or five
   elements that consumes the region exactly — trailing garbage fails.
3. The first element is a plausible timestamp, the fourth is a field map
   carrying our `FIELD_CUSTOM_TYPE` discriminator and a
   `FIELD_CUSTOM_META` with a name and a sender key.
4. **The authoritative test:** recompute the address from the name in
   `FIELD_CUSTOM_META` (section 4) and require it to equal the
   destination hash the entry is stored under.

Steps 1-3 can, with residual probability, be satisfied by ciphertext.
Step 4 cannot be satisfied by accident: the entry must contain a name
that hashes to its own address. For a mailbox entry to be misclassified,
its ciphertext would have to embed a name whose channel-namespace hash
equals the mailbox's identity-derived hash — a second preimage across
disjoint namespaces. The realistic false-positive is an entry deliberately
*constructed* to pass, and that is not a false positive at all: an entry
addressed to hash(name) that names itself correctly is a channel post,
possibly with garbage content, which an open channel admits by
definition. Misclassification can waste channel storage; it cannot expose
mailbox entries.

**Classification of closed-group entries** cannot work that way — the
payload is ciphertext and carries no name, and no amount of parsing
classifies ciphertext positively. The proposal (with section 4's caveat)
is an explicit envelope *outside* the ciphertext:

    destination(16) || tag+version || group_pubkey || group_signature
                   || ciphertext

The node classifies by recomputing the address from the embedded public
key — fixed aspects, no name needed — and requiring equality with the
entry's destination, then verifying the group signature over destination
and ciphertext against that key. Both checks need no secret. The
signature additionally means non-members cannot inject entries into a
closed group, which open channels by design cannot promise. The price is
the metadata stated in section 4: the envelope is what makes the entry
observably closed-group traffic, linkable by its public key. That trade —
classifiable but visible, versus hidden but unservable under section 3's
rule — is the crux Lew should weigh when reviewing this proposal.

**What the signature proves.** Every post carries a signature, and the
sender's public key travels in `FIELD_CUSTOM_META` because
`unpack_from_bytes` (`LXMessage.py:747`) resolves the sender through
`RNS.Identity.recall` (`LXMessage.py:776`) and leaves the signature
unverified when that returns nothing (`LXMessage.py:809-816`), which in a
public channel is the common case. Verifying the in-message key against
the source hash makes a channel self-supporting rather than dependent on
whether an announce happened to arrive. But this must not be oversold, as
an earlier draft did with "cryptographically attributable": the key is
attacker-chosen material that travels *in the message*, and the source
hash derives from it. Verification therefore proves key continuity — two
posts verified against the same key were made by the same key holder —
and nothing about who that holder is. An impersonator mints a fresh
keypair, copies a display name, and every one of their posts verifies.
The remedy is the same as for look-alike channel names: the client
displays a short hash prefix of the author identity beside the display
name, so two authors who read alike are visibly distinct. Key continuity
plus visible key prefixes is the honest offer; it is more than nothing,
and less than identity.

The key costs 64 bytes; on narrow links a client may send it only for
the first message per author per time window.


## 6. The directory, and creating a channel

A node cannot invert a hash, so it cannot enumerate channels from stored
entries alone. It does not have to: for open channels the name travels
inside the message and is verified on arrival, so every node learns of a
channel the moment its first message passes through. The directory builds
itself out of traffic, network-wide, with no registry, no gossip protocol
and no configuration.

Closed groups never appear in any directory. Precisely: the node knows a
classified group's address, its public key and its traffic volume — it
can enumerate *that a group exists* — but it never learns a name, and the
directory lists names.

**Creating a channel is therefore not an operation.** A client picks a
name and posts. The channel exists, and it appears in the directory of
every node that sees the message. There is nothing to register and no one
to ask.

The cost is that names are unowned, so squatting and homoglyph confusion
("general" against "genera1") are possible. An earlier draft called this
"the same trade Meshtastic makes", and that equivalence is false: a
Meshtastic channel name collides only within RF range, while this
namespace is worldwide — there is exactly one "general" for the entire
connected network, and whoever posts to it first shapes it for everyone.
The global namespace makes squatting strictly worse than in the systems
this feature borrows from, and two mitigations follow. Directory entries
must be ranked and bounded by activity, not merely accumulated, or the
listing becomes a spam surface. And the client must display a short hash
prefix beside the name, so two channels that read alike are still
visibly distinct.


## 7. The retrieval protocol

Three request handlers are registered on the `lxmf.propagation`
destination alongside the existing `/offer` and `/get`. Python nodes do
not know them and will fail the request, which doubles as the fallback
when discovery is stale.

`/channel/list` returns the directory: name, hash prefix, an activity
figure and the timestamp of the most recent post. Open channels only,
paged and bounded.

`/channel/get` takes a **channel name**, not a hash, plus an optional
cursor. The node derives the hash itself and serves only entries
classified per section 5. Passing the name rather than the address is
what keeps section 3's rule enforceable for open channels: there is no
name that produces a mailbox address, so the endpoint cannot be aimed at
one.

`/channel/auth` admits a closed-group reader, under section 4's proposal,
by proof of key possession. The client presents the group public key; the
node recomputes the address from it and, if it holds classified entries
for that address, issues a random nonce; the client returns a signature
over the nonce and the link ID under the group key; the node verifies
against the presented key and serves that one address for the lifetime of
the link. At no point does a group secret reach the node, and knowledge
of an address alone gets nothing: the address must be *derived* from a
key the client demonstrably holds, which is the verifiable binding the
earlier HMAC design lacked (section 4). Since closed-group entries are
also positively classified by their envelope, section 3's rule holds on
this path with or without the auth step; what auth adds is that
non-members cannot remotely harvest a group's ciphertext and traffic
pattern through our own endpoint. The local view of a node operator is
out of scope for auth, as section 3 states.

Retrieval never deletes. Python's `/get` treats the client's "have" list
as a purge instruction (`LXMRouter.py:1509-1514`), which is right for a
mailbox and fatal for a channel, since the first reader would empty it
for everyone. Channel entries on our nodes expire on their own TTL and a
per-channel ring buffer instead.


## 8. Discovery

Position 6 of the propagation node announce is a metadata dict, and
`pn_announce_data_is_valid` checks only that it is a dict
(`LXMF.py:224-244`). Unknown keys pass validation untouched; the
receiving router stores the dict on the peer object (`LXMRouter.py:2018`)
and otherwise ignores what it does not know. LXMF reserves
`PN_META_CUSTOM = 0xFF` (`LXMF.py:138`) for exactly this.

Our node advertises channel support under a namespaced key there:
protocol version, which channel types it serves, directory size, and the
minimum stamp cost it requires for channel posts. Because propagation
node announces travel over the ordinary announce mechanism, every client
in the mesh sees them. `lnmsg` collects them, filters on the capability
key, and selects by `hops_to`. That is the automatic discovery the
feature needs, and it costs nothing upstream.

One caveat, from the LXMF source itself: the metadata fields "may be
highly unstable in allocation and availability until the version 1.0.0
release, so use at your own risk until then, and expect changes!"
(`LXMF.py:128-131`). The mechanism is sound, the field numbering is not
guaranteed. Our entry therefore carries its own version field and the
client must tolerate absence and garbage.


## 9. Storage, cost and fairness

Two directions of cost, and both need a deliberate answer.

**Inbound.** A node participating in the peer network receives
everything, not only channels. Ours will store and forward the entire
LXMF propagation traffic of the reachable network. That figure should be
measured on a real node before the server is built, since it decides the
hardware floor.

**Outbound.** Our channel messages come to rest on every Python node in
the network, for up to the thirty-day `MESSAGE_EXPIRY`
(`LXMRouter.py:38`, applied from receive time in `clean_message_store`,
`LXMRouter.py:1144-1163`), and nobody ever collects a channel post, so
until expiry only storage pressure removes them. Under pressure the
eviction pass weighs every entry by `get_weight`
(`LXMRouter.py:1056-1067`) — priority weight times age times size — and
evicts the heaviest first (`LXMRouter.py:1188-1196`). The stamp value is
stored with each entry and eviction never consults it. An earlier draft
claimed the opposite, citing this very code, and built its politeness
story on posts carrying deliberately low stamps so channels would be
"first evicted". That claim is withdrawn: no such lever exists. What the
weighting actually does is evict big-and-old first and spare
operator-prioritised destinations, so an active channel competes with
other people's mail on exactly those terms — it *does* displace mail on
foreign nodes under pressure. The honest levers are the remaining ones:
keep posts small (size is a linear factor in the weight), keep volume
moderate, and keep the footprint visible and measured, which is section
10's bargain rather than a technical trick.

**Stamps are admission control, not eviction control.** Every post must
carry the proof of work section 2 describes — at least 13 bits against
any conforming node, 16 to clear every default configuration — and our
clients mint to the announced cost of the node they post through. That is
a mandatory floor the network enforces, not a knob of ours. Above that
floor, a channel may declare its own minimum stamp cost as a spam
barrier, and here the earlier draft's contradiction has to be resolved
rather than papered over: one stamp value cannot be pulled low for
eviction politeness and high for spam defence at once. The resolution is
that the politeness direction never existed (see above), so the stamp is
pulled in one direction only — upward, as a cost on posting. Its
enforcement is also honestly narrower than "the network": our nodes
refuse to serve entries below the channel's declared minimum and our
clients refuse to display them, but a Python node stores and floods
anything at or above its own floor regardless. A per-channel stamp floor
filters what readers see through us; it does not keep spam off the
carrier.

Channel entries on our own nodes live in a separate quota from mailbox
entries, so that a busy channel can never crowd out the mailbox function
we also promise to provide.

**The flood is global and the use case is local.** This is a real
mismatch and it gets recorded as a weighed decision, not smuggled past.
The mechanism puts every post of every channel onto every propagation
node in the connected network for up to thirty days, while the motivating
use case — "type a name, talk to whoever is around" — is local chat.
Alternatives were considered. Carrying channels only on our own nodes
avoids imposing on foreign operators but gives up the free transport that
is this design's entire reason to exist, and is kept as the degraded mode
in section 10 rather than the default. Regionally scoped names
("hb.general") reduce *reader* collision but change nothing about
propagation — the carrier does not read names, every post still floods
globally. A sender-chosen TTL shorter than thirty days does not exist in
the protocol: expiry is the storing node's constant, counted from receive
time, and nothing in the message can lower it. So the trade is accepted
for v1 with open eyes: global flood is the price of zero-infrastructure
distribution, it is bounded by the stamp floor and the smallness of chat
messages, and the inbound measurement above doubles as the check on
whether the price is as low as this paragraph assumes. If the measured
numbers say otherwise, the decision gets revisited, not defended.


## 10. The compatibility bargain

Everything above rests on Python nodes accepting without inspection and
flooding without filtering, gated only by stamps and peering keys. That
is a factual property of the current implementation, not a promise
anyone made us. A single upstream commit restricting storage to
destinations that have been announced would end the free-transport half,
and from a node operator's perspective that would be an entirely
reasonable change.

This cannot be secured technically, only socially. We say plainly what we
are doing, we make it identifiable in the announce metadata, and we keep
our footprint on foreign nodes visibly small. Doing it quietly and being
found out later closes the route permanently.

**The degradation plan, decided now rather than improvised later.** If
upstream starts classifying or rejecting channel entries — requiring
announced destinations, parsing payloads, or filtering our discriminator
— the design degrades to an overlay instead of breaking. Our nodes are
full propagation nodes that peer with each other statically, not only by
autopeer, so channel distribution continues over our own peerings with
the same sync protocol; clients already post and read through our nodes,
so nothing changes for them. What is lost is exactly the free transport:
reach shrinks from every propagation node to the connected set of our
nodes plus whatever Python nodes still carry unclassified traffic. The
mailbox function is untouched either way, because our nodes are
conforming propagation nodes first. To notice the change when it happens
rather than months later, a canary: periodically post a test channel
message through a Python node and confirm arrival at one of ours over a
Python-only path; when that stops, the assumption behind sections 2 and 9
has expired and the overlay mode becomes the documented default.

The conformance obligation is the ordinary one: our node has to be a
correct propagation node first and a channel node second. Stamp
validation, peering keys, sync limits, throttling and announce format all
have to match Python exactly, or autopeering will not happen and none of
this runs. Channels are an addition to a compatible node, never a
deviation from one.


## 11. Open questions

- The inbound storage and bandwidth figure for full peer participation
  (section 9) is unmeasured. It should be measured before the server is
  designed, not after — it decides both the hardware floor and whether
  section 9's global-flood trade is as cheap as assumed.
- The closed-group proposal in sections 4, 5 and 7 needs its own review:
  whether the shared-keypair group secret is acceptable, and whether the
  envelope's metadata cost (visible group existence, linkable public
  key) is a price the use case can pay.
- Whether closed groups need key rotation, and what happens to the
  address when a member leaves, given that the address is derived from
  the group keypair.
- Directory ranking: what activity measure, over what window, and how
  large a listing before it needs paging or filtering.
- Whether channel messages should carry a `FIELD_THREAD` equivalent, so
  replies can be shown threaded rather than flat.
