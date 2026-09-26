# lnstest(1)

## NAME

lnstest -- Reticulum test and diagnostics tool

## SYNOPSIS

**lnstest** [**-c** *dir*] [**-v**] *command* [*args*...]

## DESCRIPTION

**lnstest** is the test and diagnostics tool for the Leviculum
Reticulum stack. It drives integration self-tests, collects diagnostic
bundles from a running daemon, manages identities, and opens interactive
sessions. It works against either **lnsd** or Python **rnsd** through the
shared-instance interface. For file transfer use **lncp**(1).

## GLOBAL OPTIONS

**-c**, **--config** *dir*
:   Path to the Reticulum configuration directory.

**-v**, **--verbose**
:   Enable verbose logging.

**--corrupt-every** *n*
:   Fault injection: corrupt roughly one byte per *n* bytes written to TCP. For **selftest**, corruption is held back until mutual discovery has completed, so that announces cross a clean stream.

## COMMANDS

### lnstest identity generate [**-o** *file*]

Generate a new Reticulum identity and write it to *file*.

### lnstest identity show *file*

Show the hash and public keys of the identity in *file*.

### lnstest connect *addr*

Open an interactive session to a Reticulum daemon at *addr* (host:port). Supports link establishment, message exchange, and announce discovery. Type `/help` in the session for available commands.

### lnstest selftest *target* [*target*]

Run integration self-tests through one or two relay nodes. Tests link establishment, channel data, ratchet operation, and bulk transfer.

Options:

**--duration** *seconds*
:   Test duration (default: 180).

**--rate** *n*
:   Messages per second per direction (default: 1).

**--mode** *mode*
:   Which phases to run: all, link, packet, ratchet-basic, ratchet-enforced, bulk-transfer, ratchet-rotation (default: all).

Every single-packet phase waits for what is still in flight before it reads the receive counter. On a radio link that wait must be sized from the link, and the tool has no radio of its own, so give it the global **-c**/**--config** pointing at the config directory of the daemon that owns the radio: it reads that daemon's `interface_stats` and sizes each window from the reported on-air bitrate and pre-TX jitter ceiling. Without it — or against a daemon reporting no radio — the phases keep a fixed wait, and the run prints which state it is in.

### lnstest diag

Collect a self-contained diagnostic bundle from a running **lnsd** (or **rnsd**) for attaching to bug reports: versions/build, the secret-redacted config and configured interfaces, the daemon's live view via the shared-instance RPC (interface stats, path table, link count), best-effort system info, and an event-log pointer. Printed to stdout by default. Use the global **-c**/**--config** to point at the daemon's config directory.

Secrets are redacted — IFAC `passphrase` and `networkname` never appear, and the node identity private key (`storage/transport_identity`) is never read into the bundle (it is used only to derive the RPC authkey). Queries the daemon doesn't support (e.g. when run against Python **rnsd**) are reported as unavailable rather than failing.

Options:

**--output** *path*
:   Write the bundle to *path* instead of stdout (a one-line confirmation is printed to stderr).

**--instance-name** *name*
:   Shared-instance name to query (default: from the config, else `default`).

**--event-log** *path*
:   Tail this structured event-log file into the bundle (when **lnsd** was started with `LEVICULUM_EVENT_LOG` set).

**--no-rpc**
:   Skip the daemon RPC queries; emit only the config, versions, and system sections.

## EXAMPLES

Generate a new identity:

    lnstest identity generate -o my_identity

Run a self-test through a relay node:

    lnstest selftest 192.0.2.10:4965 --mode link --duration 60

Collect a diagnostic bundle from a running daemon:

    lnstest diag -c /etc/reticulum --output /tmp/lnstest-diag.txt

## SEE ALSO

**lnsd**(1), **lncp**(1), **lnstatus**(1)
