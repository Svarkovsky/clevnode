# lnstest

`lnstest` is the Reticulum test and diagnostics tool. It manages
identities, runs an interactive session against a daemon, exercises the
stack with a self-test, and collects diagnostic bundles. It works
against either `lnsd` or Python `rnsd` through the shared-instance
interface. For file transfer use the standalone [`lncp`](lncp.md) tool.

```text
Reticulum test and diagnostics tool

Usage: lnstest [OPTIONS] <COMMAND>

Commands:
  identity    Identity management
  selftest    Run integration self-test through relay node(s)
  connect     Interactive session: connect to rnsd and enter command loop
  diag        Collect a diagnostic bundle from a running lnsd (or rnsd) for bug reports
  help        Print this message or the help of the given subcommand(s)

Options:
  -c, --config <CONFIG>                Config directory of the daemon to ask
  -v, --verbose                        Enable verbose logging
      --corrupt-every <CORRUPT_EVERY>  Corrupt ~1 byte per N bytes on TCP write (fault injection)
  -h, --help                           Print help
  -V, --version                        Print version
```

`-c/--config`, `-v/--verbose`, and `--corrupt-every` are global flags
available on every subcommand. `--config` names a daemon's *config
directory* — the one `lnsd --config` was given — not a file: `diag` and
`selftest` both read the config and the identity under it to reach that
daemon's shared instance. `--corrupt-every` is a fault-injection tool for
testing and should be left off in normal use.

## identity

Manage Reticulum identities. An identity file holds the 64-byte private
key (X25519 + Ed25519); the public half and the 16-byte hash are derived
from it.

```text
Usage: lnstest identity <COMMAND>

Commands:
  generate  Generate a new identity
  show      Show identity information
```

### identity generate

Creates a fresh identity. With `-o/--output FILE` the private key is
written to the file and the hash is printed; without it, the hash and
public key are printed and nothing is saved (`lnstest.rs:1033-1052`).

```sh
lnstest identity generate -o my-identity.bin
```

```text
Generated new identity
Hash: 0123456789abcdef0123456789abcdef
Saved to: my-identity.bin
```

Without `-o`, the public key is printed instead of being saved
(`lnstest.rs:1047-1051`):

```sh
lnstest identity generate
```

```text
Generated new identity
Hash: 0123456789abcdef0123456789abcdef
Public key: <64 hex bytes>
```

### identity show

Loads a saved identity file and prints its path, hash, and public key
(`lnstest.rs:1054-1066`):

```sh
lnstest identity show my-identity.bin
```

```text
Identity: my-identity.bin
Hash: 0123456789abcdef0123456789abcdef
Public key: <64 hex bytes>
```

## File transfer

`lnstest` has no file-copy subcommand. Use the standalone
[`lncp`](lncp.md) tool, the drop-in for Python's `rncp`, which attaches
to a running `lnsd` (or `rnsd`) through the shared instance.

## connect

Open an interactive session against a running daemon (`lnsd` or `rnsd`)
and enter a command loop. The address is the daemon's TCP interface
(`host:port`); `lnstest` verifies TCP connectivity before building the node
(`lnstest.rs:486-488`).

```text
Usage: lnstest connect [OPTIONS] <ADDR>

Arguments:
  <ADDR>  Address of the rnsd to connect to (host:port)

Options:
  -c, --config <CONFIG>      Config directory of the daemon to ask
      --identity <IDENTITY>  Path to identity file (default: generate ephemeral)
```

With no `--identity`, an ephemeral identity is generated for the session
(`lnstest.rs:497-501`). On connect, the session announces itself and prints
its identity and destination hashes (`lnstest.rs:537-547`):

```text
Identity: <hash>
Destination: <hash>
Announced as lnstest-cli
Type /help for commands.
>
```

### Interactive commands

The command loop accepts these (`lnstest.rs:592-820`):

| Command | Action |
|---------|--------|
| `/peers` | List discovered destinations |
| `/link <hash>` | Initiate a link to a destination (32-char hex) |
| `/target <hash>` | Set a single-packet destination (32-char hex) |
| `/untarget` | Clear the single-packet target |
| `/send <msg>` | Send data on the active link or to the target |
| `/close` | Close the active link |
| `/announce` | Re-announce this destination |
| `/quiet` | Hide announce/path messages |
| `/verbose` | Show announce/path messages |
| `/status` | Show node status (identity, destination, paths, peers) |
| `/help` | Show this help |
| `/quit` | Exit |
| `<bare text>` | Send as data on the active link or to the target |

## selftest

Run an end-to-end self-test through one or two relay nodes you control.
The relay addresses are given as `host:port`.

```text
Usage: lnstest selftest [OPTIONS] [TARGETS]...

Arguments:
  [TARGETS]...  Address(es) of relay node(s) (host:port). One or two addresses

Options:
  -c, --config <CONFIG>
          Config directory of the daemon to ask
      --duration <DURATION>
          Test duration in seconds [default: 180]
      --rate <RATE>
          Messages per second per direction [default: 1]
      --mode <MODE>
          Which test phases to run [default: all]
      --discovery-timeout <DISCOVERY_TIMEOUT>
          Discovery timeout in seconds (Phase 2: mutual path discovery) [default: 60]
```

The `--mode` flag selects which phases run; the values are `all`,
`link`, `packet`, `ratchet-basic`, `ratchet-enforced`,
`bulk-transfer`, and `ratchet-rotation` (default `all`). `--duration`
defaults to 180 seconds, `--rate` to 1 message per second per
direction, and `--discovery-timeout` to 60 seconds.

```sh
# Full self-test through one relay
lnstest selftest 192.0.2.10:4965

# Just the link phase, two relays, shorter run
lnstest selftest --mode link --duration 60 192.0.2.10:4965 192.0.2.11:4965
```

### Sizing the drain window on a slow link

Every single-packet phase waits for what is still in flight before it
reads the receive counter. On a radio link that wait has to be sized
from the link: ten 147-byte frames over a 2734 bps LoRa link need five
seconds of air, and a fixed sleep shorter than that counts the frames
still on the air as lost.

The tool has no radio of its own — it is a TCP client, usually two hops
from one — so it asks the daemon that owns the radio. Give it that
daemon's config directory:

```sh
lnstest -c /root/.reticulum selftest 127.0.0.1:4242 peer:4242 --mode ratchet-basic
```

It resolves the shared instance from that directory, reads
`interface_stats`, and sizes each phase's window from the reported
on-air bitrate and pre-TX jitter ceiling of the most constraining radio
interface. The run says which state it is in, on its own line:

```text
[selftest] Link sizing: the daemon's `RNodeInterface[/dev/ttyUSB0]` (2734 bps, jitter ceiling 2926 ms)
[selftest] Phase 6: drain budget from the daemon's `RNodeInterface[…]` (2734 bps, …): \
10 frames x 147B at 2734 bps = 6.1s air (payload 5.1s +20% preamble/header/medium access) \
+ 2.9s handover (interface pre-TX jitter ceiling) = 9.1s
```

Without `-c`, against a daemon that reports no radio, or against one
that does not report a jitter ceiling (a Python `rnsd`, or an older
`lnsd`), it degrades rather than guessing: the reason is printed on the
same line, and the phase falls back to the fixed wait, or to the
airtime term alone when only the ceiling is missing.

## diag

Collect a self-contained diagnostic bundle from a running daemon for bug
reports. `diag` queries the shared-instance RPC for the daemon's live
view (interface stats, path table, link count), bundles it with the
secret-redacted config, version and build info, and system info, and
prints to stdout (or to a file with `--output`).

```text
Usage: lnstest diag [OPTIONS]

Options:
  -c, --config <CONFIG>
          Config directory of the daemon to ask
      --output <OUTPUT>
          Write the bundle to this path instead of stdout
      --instance-name <INSTANCE_NAME>
          Shared-instance name to query (default: from config, else "default")
      --event-log <EVENT_LOG>
          Tail this structured event-log file into the bundle
      --no-rpc
          Skip the daemon RPC queries; emit only config / versions / system
```

`--no-rpc` (`lnstest.rs:419-421`) skips the daemon queries — useful for
checking config parse status when the daemon is down.

A bundle is assembled from these sections in order (`diag.rs:63-190`):

- **Versions / build** — `lnstest` version, build profile, target. (The
  daemon version is not exposed by the RPC; check the daemon's startup
  log if needed.)
- **Config** — config dir and file, parse status, then the *effective*
  config rendered as TOML with secrets redacted. The raw file is never
  included.
- **Interfaces (configured)** — each configured interface from the
  parsed config.
- **Daemon view (shared-instance RPC)** — instance name, RPC socket
  path `\0rns/<name>/rpc`, and live `interface_stats`, `path_table`,
  and `link_count` queries.
- **System** — OS, kernel, distro, and the daemon's pid / RSS / open
  fds.
- **Recent events** — tail of the structured event log if
  `--event-log` is given.

A trimmed bundle (your transport id, paths, and counters will differ):

```text
===== Leviculum diagnostic bundle =====

----- Versions / build -----
lnstest version: 0.7.0
build profile: release
target: x86_64 / linux
daemon version: not exposed by the shared-instance RPC ...

----- Config -----
config dir:  /etc/reticulum
config file: /etc/reticulum/config
config file: present, parsed OK

Effective config (TOML, secrets redacted; the raw file is NOT included
because it may contain secrets):
[reticulum]
enable_transport = true
shared_instance = true
instance_name = "default"
...

----- Daemon view (shared-instance RPC) -----
instance name: default
RPC socket:    \0rns/default/rpc
authkey:       derived from /etc/reticulum/storage/transport_identity (not shown)

## interface_stats
transport id: 0123456789abcdef0123456789abcdef
daemon uptime: 12m 34s (754s)
interfaces (3):
  - Shared Instance[rns/default]  type=LocalServerInterface status=up rxb=0 txb=0 clients=1
  - AutoInterface[Default Interface/eth0/aabbccdd]  type=AutoInterface status=up rxb=482 txb=917 peers=2
  - TCPInterface[RNS TCP Node Germany 002/193.26.158.230:4965]  type=TCPClientInterface status=up rxb=14211 txb=8332

## path_table
known paths: 7
[ ... JSON array of {hash, via, hops, expires, ...} ... ]

## link_count
relayed links: 0

----- System -----
os: linux  kernel: 6.12.73+deb13-amd64
distro: Debian GNU/Linux 13 (trixie)
lnsd pid: 12345
lnsd VmRSS: 18432 kB
lnsd open fds: 27

----- Recent events -----
No structured event-log file specified ...

===== end of diagnostic bundle =====
```

### Secret redaction

The bundle is designed to be safe to attach to a public tracker. IFAC
`passphrase` and `networkname` are redacted before the config is
serialised, and the node's private key is never read into the bundle —
only its hash is used internally to derive the RPC authkey
(`diag.rs:119-128`, `162-168`). The bundle still contains your
hostnames, configured TCP targets, byte counters, and known-paths table,
so review it once before posting if your topology is sensitive.

```sh
lnstest diag --config /etc/reticulum --output /tmp/lnstest-diag.txt
```

See the [lnsd Quickstart](../lnsd-quickstart.md) for how to read the
bundle as a health check.

## Network status, paths, and probing

`lnstest` deliberately does not reimplement Python's `rnstatus`,
`rnpath`, or `rnprobe`. For a running daemon's status, paths, and
interfaces, read the `interface_stats` and `path_table` sections of
`lnstest diag`, or point the Python `rnstatus` / `rnpath` / `rnprobe`
tools at the same shared instance — they attach to `lnsd` transparently.
