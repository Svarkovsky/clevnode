# lncp

`lncp` is the Reticulum file-transfer tool. It sends a file to a
destination or listens for incoming transfers, and is wire-compatible
with Python's `rncp` — an `lncp` listener accepts an `rncp` sender and
vice versa.

```text
Reticulum File Transfer Utility

Usage: lncp [OPTIONS] [FILE] [DESTINATION]

Arguments:
  [FILE]         File to send (send mode)
  [DESTINATION]  Destination hash, 32 hex characters (send mode)

Options:
      --config <CONFIG>   Path to alternative Reticulum config directory
  -v, --verbose...        Increase verbosity
  -q, --quiet...          Decrease verbosity
  -l, --listen            Listen for incoming transfer requests
  -w <TIMEOUT>            Fetch / transfer phase timeout in seconds
  -s, --save <SAVE>       Save received files in specified path
  -O, --overwrite         Allow overwriting received files
  -n, --no-auth           Accept requests from anyone
  -b <ANNOUNCE_INTERVAL>  Announce interval (-1=none, 0=once at startup, N=every N sec) [default: 0]
  -p, --print-identity    Print identity and destination info and exit
  -i <IDENTITY>           Path to identity file to use
  -S, --silent            Fully silent: no progress output and no log output at all (equivalent to -qq)
  -C, --no-compress       Disable automatic compression
  -f, --fetch             Fetch file from remote listener
  -F, --allow-fetch       Allow authenticated clients to fetch files
  -j, --jail <JAIL>       Restrict fetch requests to specified path
  -P, --phy-rates         Display physical layer transfer rates
  -a <ALLOWED>            Allow identity hash (can be specified multiple times)
  -h, --help              Print help
  -V, --version           Print version
```

## Modes

### Send

Give a `FILE` and a 32-hex-character `DESTINATION` hash. `lncp`
establishes a link to the destination and transfers the file:

```sh
lncp report.pdf 0123456789abcdef0123456789abcdef
```

The file is compressed automatically unless you pass `-C/--no-compress`.

### Listen

`-l/--listen` waits for incoming transfer requests. The listener prints
its own destination hash so the sender knows where to aim:

```sh
lncp -l -s ~/incoming
```

### Fetch

`-f/--fetch` pulls a file *from* a remote listener instead of pushing to
it (the listener must allow this with `-F/--allow-fetch`).

## Options

| Option | Meaning |
|--------|---------|
| `--config <DIR>` | Use an alternative Reticulum config directory instead of the default lookup. |
| `-v/--verbose`, `-q/--quiet` | Raise / lower log verbosity (stackable). |
| `-l/--listen` | Listen for incoming transfer requests. |
| `-w <TIMEOUT>` | Fetch/transfer phase timeout in seconds, counted after the link is established. Default: no timeout — the transfer runs to completion or until interrupted. Slow transports (LoRa) need no artificial cap; set this only for a hard wall-clock bound. |
| `-s/--save <PATH>` | Save received files in this directory (listen mode). |
| `-O/--overwrite` | Allow overwriting existing received files. |
| `-n/--no-auth` | Accept requests from anyone (overrides `-a`). |
| `-b <INTERVAL>` | Announce interval: `-1` never, `0` once at startup, `N` every N seconds. Default: `0`. |
| `-p/--print-identity` | Print the destination hash and identity hash, then exit. |
| `-i <IDENTITY>` | Use this identity file instead of the default. |
| `-S/--silent` | Fully silent: no progress and no log output (equivalent to `-qq`). |
| `-C/--no-compress` | Disable automatic compression. |
| `-f/--fetch` | Fetch a file from a remote listener (instead of sending). |
| `-F/--allow-fetch` | Allow authenticated clients to fetch files (listen mode). |
| `-j/--jail <PATH>` | Restrict fetch requests to this directory (use with `-F`). |
| `-P/--phy-rates` | Display physical-layer transfer rates. |
| `-a <HASH>` | Allow a specific identity hash; repeatable to allow several. |

A few options only make sense in listen mode and warn otherwise:
`-F/--allow-fetch` warns when no `-l` is given, and `-j/--jail` warns
without `-F` (`lncp.rs:199-204`). `-n/--no-auth` overrides any `-a`
allow-list (`lncp.rs:213-214`).

### Identity and authorisation

`-p/--print-identity` loads (or generates) the identity and prints the
`rncp` receive destination hash followed by the identity hash, then
exits (`lncp.rs:334-352`):

```sh
lncp -p
```

```text
0123456789abcdef0123456789abcdef
Identity  : fedcba9876543210fedcba9876543210
```

By default a listener only accepts senders whose identity hash you have
allowed with `-a` (repeatable). `-n/--no-auth` drops that check and
accepts anyone. An `-a` hash must be 32 hex characters / 16 bytes
(`lncp.rs:314-332`).

## Examples

### Send a file

On the receiver, start a listener and note its destination hash:

```sh
lncp -l -s ~/incoming -O
```

On the sender, transfer the file to that hash:

```sh
lncp ./report.pdf 0123456789abcdef0123456789abcdef
```

### Listen and receive with authorisation

Allow only one known sender, saving into `~/incoming` and showing
physical-layer rates:

```sh
lncp -l -s ~/incoming -a fedcba9876543210fedcba9876543210 -P
```

The sender finds its own identity hash with `lncp -p`.

`lncp` needs a running daemon (`lnsd` or `rnsd`) on the same shared
instance to reach the mesh — see the
[lnsd Quickstart](../lnsd-quickstart.md).
