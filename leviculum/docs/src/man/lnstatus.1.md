# lnstatus(1)

## NAME

lnstatus -- Reticulum network stack status

## SYNOPSIS

**lnstatus** [*options*] [*filter*]

## DESCRIPTION

**lnstatus** displays the status of the interfaces on a running Reticulum daemon. It is compatible with Python's **rnstatus** and produces the same per-interface layout. It connects to a running daemon (**lnsd** or **rnsd**) via shared instance IPC, querying `interface_stats` (and `link_count` for **-l**), so `lnstatus | diff rnstatus` against the same daemon passes.

Without a *filter*, all up interfaces are shown. Give a *filter* string to only display interfaces whose name contains it.

With **-R** it queries a remote transport instance over a link, the way `rnstatus -R` does, and feeds the result to the same renderer, so remote and local output match. With **-d**/**-D** it reads the local discovered-interface registry over the RPC and renders the `rnstatus` discovered layout.

## OPTIONS

*filter*
:   Only display interfaces whose name contains this string.

**--config** *dir*
:   Path to alternative Reticulum configuration directory.

**--instance-name** *name*
:   Shared-instance name to query. Defaults to the value from the configuration file, otherwise `default`.

**-a**, **--all**
:   Show all interfaces, including those that are down.

**-A**, **--announce-stats**
:   Show announce statistics.

**-P**, **--pr-stats**
:   Show path request statistics.

**-B**, **--burst**
:   Only show interfaces with active bursts.

**-l**, **--link-stats**
:   Show link statistics: the number of entries in the daemon's transport link table, i.e. the links it relays (queries `link_count` from the daemon, the same value `rnstatus -l` reads).

**-t**, **--totals**
:   Display traffic totals.

**-s**, **--sort** *key*
:   Sort interfaces by *key*: `rate`, `traffic`, `rx`, `tx`, `rxs`, `txs`, `announces`, `arx`, `atx`, `prx`, `ptx`, or `held`.

**-r**, **--reverse**
:   Reverse the sort order.

**-j**, **--json**
:   Output in JSON format.

**--tables**
:   Add the transport's internal tables to the JSON output as a
    `transport_tables` object. Requires **-j**; not available with **-R** or
    **-d**/**-D**. Leviculum extension — `rnstatus` has no counterpart, and a
    daemon that does not implement it (a Python `rnsd`, or an older `lnsd`)
    causes the key to be omitted, with a note on stderr and exit status 0.
    See **TRANSPORT TABLES** below.

**-N**, **--identities**
:   List every identity the daemon has learned from announces, one row per
    announced destination, plus a `derived:` line with the `lxmf.delivery`
    and `rnstransport.probe` destinations computed from the identity hash.
    Leviculum extension — `rnstatus` has no counterpart, and a daemon that
    does not implement the query (a Python `rnsd`, or an older `lnsd`)
    causes an error and exit status 2. With **-j** the raw response is
    printed instead. Not available with **-R**, **-d**/**-D**, **--tables**
    or **-m**. See **IDENTITY LISTING** below.

**-m**, **--monitor**
:   Continuously monitor status, clearing and redrawing on each interval.

**-I**, **--monitor-interval** *seconds*
:   Refresh interval for monitor mode (default: 1).

**-v**, **--verbose**
:   Increase verbosity. Repeat for more detail.

**--version**
:   Print version and exit.

**-R** *hash*
:   Transport identity hash of a remote instance to query instead of the local one.

**-i** *file*
:   Identity used for remote management.

**-w** *seconds*
:   Timeout before giving up on remote queries.

**-d**, **--discovered**
:   List interfaces discovered on the network.

**-D**
:   Show details and config entries for discovered interfaces.

## EXIT STATUS

**0**
:   Success.

**1**
:   No shared RNS instance available to get status from (could not derive the RPC authkey).

**2**
:   The status query failed.

**20**
:   Remote management (**-R**) was requested but the management identity is missing or unusable.

## EXAMPLES

Show all interfaces:

    lnstatus

Show announce and path request statistics for interfaces named like `eth`:

    lnstatus -A -P eth

Sort interfaces by traffic, most first:

    lnstatus -s traffic -r

Continuously monitor, refreshing every two seconds:

    lnstatus -m -I 2

Emit machine-readable JSON:

    lnstatus -j

Emit JSON with the transport's tables included:

    lnstatus -j --tables

List the identities heard from announces, with derived destinations ready to
paste into **lnprobe**:

    lnstatus -N

## IDENTITY LISTING

`rnpath -t` shows destination hashes only, but probing a remote transport node
needs its `rnstransport.probe` destination, which is derived from its identity
hash — and the daemon knows that identity, because the announce carried the
public key. **-N** exposes it: one row per announced destination the daemon
still holds, with the identity hash, the announced destination hash, the name
(only when it matches an aspect the daemon registered itself; `?` otherwise —
never guessed), and the live path toward it (`hops`, `via` as
interface/next-hop, `last seen`). Columns without a live path show `-`.

Under each row a `derived:` line prints the destinations computed from the
identity hash as `sha256(sha256(name)[:10] + identity_hash)[:16]` for the two
names that matter in practice: `lxmf.delivery` and `rnstransport.probe`.
Hashes are printed in full so they can be pasted into `lnprobe` or `rnprobe`.

The inventory is the daemon's announce cache (Python's equivalent store is
`Identity.known_destinations`), which is bounded by the announce-cache
cleaning the daemon already performs; an identity whose cached announce has
been evicted no longer appears.

## TRANSPORT TABLES

With **--tables**, the **-j** object gains one additional key,
`transport_tables`. Nothing else about the output changes, so anything that
parses `lnstatus -j` today keeps working.

The object holds one key per table, each a list of rows:

`path_table`
:   Destinations this node knows a route to. Keys `hash`, `timestamp`, `via`,
    `hops`, `expires`, `interface` are the same keys, with the same units, that
    Python's own `path_table` RPC returns (`Reticulum.get_path_table`). Added:
    `announce_emitted`.

`reverse_table`
:   Where to send the reply to a packet this node forwarded: `hash`,
    `receiving_interface`, `outbound_interface`, `timestamp`.

`link_table`
:   Links this node **relays**: `link_id`, `timestamp`,
    `next_hop_interface`, `remaining_hops`, `receiving_interface`, `hops`,
    `destination_hash`, `validated`, `proof_timeout`.

`announce_table`
:   Announces held for deferred rebroadcast: `hash`, `timestamp`,
    `retransmit_timeout`, `retries`, `receiving_interface`, `hops`,
    `packet_length`, `local_rebroadcasts`, `block_rebroadcasts`,
    `attached_interface`.

`announce_cache`
:   Known destinations whose last announce is still held: `hash`,
    `packet_length`, `retained`, `last_used`.

`tunnels`
:   Reconnectable peers and the paths held against them: `tunnel_id`,
    `interface`, `expires`, and `paths`, each path carrying `hash`, `hops`,
    `via`, `expires`, `timestamp`, `announce_emitted`.

`local_links`
:   Links this node is an **endpoint** of — not the same table as `link_table`
    above: `link_id`, `state`, `destination_hash`, `age`, `interface`.

### Which clock a timestamp is

Two questions that are easy to confuse, and are answered by two different
keys:

`timestamp`
:   **Our** clock. When this node learned or last refreshed the row, in Unix
    seconds. In `path_table` it is recovered from `expires` minus the lifetime
    that path was granted, so it is exact for a path still on the interface it
    was learned on.

`announce_emitted`
:   **The announcing node's** clock. The whole-second emission stamp that node
    wrote into its announce, as this node received it. Peers order competing
    announces for one destination by this value, so it is a claim about a
    remote machine's time, never about ours. `0` means no announce blob is
    stored for the row.

### Absent is not empty

A daemon that implements the query answers with `transport_tables` present and
its tables possibly empty. A daemon that does not implement it causes the key
to be **omitted** entirely. Test the key's presence to tell the two apart; do
not read an absent key as an empty table.

## SEE ALSO

**lnsd**(1), **lnstest**(1), **lncp**(1)
