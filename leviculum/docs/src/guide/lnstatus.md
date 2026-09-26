# lnstatus

`lnstatus` is the Reticulum status tool. It shows the interfaces of a
running daemon and their traffic, announce, path-request, and link
statistics, and is output-compatible with Python's `rnstatus` — fed the
same `interface_stats` from the same daemon, `lnstatus` and `rnstatus`
render byte-identical output, so `lnstatus | diff rnstatus` passes.

```text
Reticulum Network Stack Status

Usage: lnstatus [OPTIONS] [FILTER]

Arguments:
  [FILTER]  only display interfaces with names including filter

Options:
      --config <CONFIG>
          path to alternative Reticulum config directory
  -a, --all
          show all interfaces
  -A, --announce-stats
          show announce stats
  -P, --pr-stats
          show path request stats
  -l, --link-stats
          show link stats
  -B, --burst
          only show interfaces with active bursts
  -t, --totals
          display traffic totals
  -s, --sort <SORT>
          sort interfaces by [rate, traffic, rx, tx, rxs, txs, announces, arx, atx, prx, ptx, held]
  -r, --reverse
          reverse sorting
  -j, --json
          output in JSON format
      --tables
          add the transport's internal tables to the JSON output (requires -j)
  -R <REMOTE>
          transport identity hash of remote instance to get status from
  -i <IDENTITY>
          path to identity used for remote management
  -w <TIMEOUT>
          timeout before giving up on remote queries
  -d, --discovered
          list discovered interfaces
  -D
          show details and config entries for discovered interfaces
  -m, --monitor
          continuously monitor status
  -I, --monitor-interval <MONITOR_INTERVAL>
          refresh interval for monitor mode (default: 1) [default: 1]
  -v, --verbose...
          verbose logging (repeatable)
      --instance-name <INSTANCE_NAME>
          shared-instance name to query (default: from config, else "default")
  -h, --help
          Print help (see more with '--help')
  -V, --version
          Print version
```

`lnstatus` needs a running daemon (`lnsd` or `rnsd`) on the same shared
instance to query — see the
[lnsd Quickstart](../lnsd-quickstart.md).

## Running it against a daemon

With `lnsd` (or Python `rnsd`) running, `lnstatus` with no arguments
prints every up interface and its counters:

```sh
lnstatus
```

It resolves the daemon exactly like the other tools: the config
directory (default lookup, or `--config <DIR>`) gives the shared-instance
name and the RPC authkey. If no shared instance is reachable, it reports
`No shared RNS instance available to get status from` and exits non-zero.

Give a `FILTER` to restrict the output to interfaces whose name contains
it:

```sh
lnstatus eth
```

## Common flags

### Extra statistics

`-A/--announce-stats` and `-P/--pr-stats` add announce and path-request
columns; `-l/--link-stats` adds link counts (queried separately from the
daemon); `-t/--totals` appends traffic totals:

```sh
lnstatus -A -P -l -t
```

`-a/--all` also shows interfaces that are currently down, and
`-B/--burst` restricts the output to interfaces with active bursts.

### Sorting

`-s/--sort <KEY>` orders the interfaces by one of `rate`, `traffic`,
`rx`, `tx`, `rxs`, `txs`, `announces`, `arx`, `atx`, `prx`, `ptx`, or
`held`; `-r/--reverse` flips the order:

```sh
lnstatus -s traffic -r
```

### Monitor mode

`-m/--monitor` clears the screen and re-renders on each interval;
`-I/--monitor-interval <SECONDS>` sets the refresh period (default `1`):

```sh
lnstatus -m -I 2
```

### JSON output

`-j/--json` emits the status as JSON instead of the rendered table, for
scripting:

```sh
lnstatus -j
```

### The transport's tables

`--tables` adds one key, `transport_tables`, to that JSON object. It carries
the tables the transport maintains — `path_table`, `reverse_table`,
`link_table` (relayed links), `announce_table`, `announce_cache`, `tunnels`,
and `local_links` (links this node terminates) — so a test can assert on the
route a packet will take instead of inferring it from log lines:

```sh
lnstatus -j --tables
```

`rnstatus` has no counterpart, so the flag requires `-j` and never changes what
a reference flag prints. `lnstatus -j` on its own is exactly what it was.

Two timestamps in there answer different questions. `timestamp` is *our*
clock — when this node learned the row. `announce_emitted` is the *announcing*
node's clock — the second it stamped into its announce, which is what peers
order competing announces by.

A daemon that does not implement the query — a Python `rnsd`, or an `lnsd`
older than this flag — makes `lnstatus` omit the key, print why on stderr, and
exit 0; the status you asked for is still printed. So an **absent**
`transport_tables` key means "this daemon cannot answer", while a **present**
key with empty lists means "the tables really are empty". Check for the key
before reading it, and do not treat its absence as an empty table.

Full field lists are in [lnstatus(1)](../man/lnstatus.1.md).

## Remote status (`-R`/`-i`/`-w`)

`-R <hash>` queries a *remote* transport instance's status over a link,
the way `rnstatus -R` does, and feeds the result to the same renderer,
so remote and local output match (`run_remote` (`lnstatus.rs:422`)).
`<hash>` is the remote instance's transport identity hash (32 hex
characters). `-i <file>` names the management identity and is
mandatory; it is proven to the remote over the link, so the remote
daemon only answers if it has remote management enabled and lists that
identity as allowed. `-w <seconds>` bounds the query (default 15,
matching Python's path-request timeout). `-m` re-queries the remote on
the monitor interval, and `-j` renders the remote status as JSON:

```sh
lnstatus -R 76fe5751a56067d1e84eef3e88eab85b -i ~/.reticulum/identities/mgmt -w 30
```

## Discovered interfaces (`-d`/`-D`)

`-d` lists the interfaces this daemon has discovered on the network, in
the `rnstatus` discovered layout; `-D` renders the detailed layout with
ready-to-paste config entries (`run_discovered` (`lnstatus.rs:331`)).
Both read the local daemon's discovered-interface registry over the
shared-instance RPC and honour `FILTER` and `-j`:

```sh
lnstatus -D rnode
```

## Examples

### Full picture of a local daemon

```sh
lnstatus -a -A -P -l -t
```

### Watch one interface live

```sh
lnstatus -m -I 2 rnode
```

`lnstatus` needs a running daemon (`lnsd` or `rnsd`) on the same shared
instance to reach the mesh — see the
[lnsd Quickstart](../lnsd-quickstart.md).
