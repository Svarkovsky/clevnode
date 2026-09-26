# lnsd Quickstart for Beta Testers

This page gets you from "I have the `.deb` file" to "my node is on the
mesh and I know how to tell if it isn't", plus the one-liner you run when
something is off so the bug report has everything we need.

For the protocol itself, the upstream [Reticulum Manual](https://reticulum.network/manual/)
is the reference. This page is about getting `lnsd` running on your
machine.

## Prerequisites

- Linux, x86_64 or aarch64. (macOS and embedded targets exist but are
  out of scope for the beta `.deb` path.)
- The nightly `.deb` for your architecture. Download links are on the
  [releases page](https://codeberg.org/Lew_Palm/leviculum/releases).
  The binaries inside are statically linked against musl, so the
  package installs on Debian ≥ 9 and Ubuntu ≥ 16.04 regardless of host
  glibc.
- A few free TCP/UDP ports on your machine for the configured
  interfaces (default ports below).

You do **not** need to install Rust, Python, or Docker for the beta
flow.

## Install

```sh
sudo apt install ./leviculum-nightly-amd64.deb       # or -arm64
```

The package:

- Installs `lnsd`, `lnstest`, `lncp`, and `lnstatus` under `/usr/bin/`,
  with a man page for each.
- Creates a system user `leviculum` and a group of the same name.
- Drops a default config file at `/etc/reticulum/config` (mode 644)
  and creates the config directory `/etc/reticulum` mode 2775
  (group-writable + setgid, so files created inside it inherit the
  `leviculum` group).
- Enables and starts the `lnsd.service` systemd unit.

For the native tools (`lnstest`, `lncp`, `lnstatus`) and Python tools (`rnstatus`,
`rnpath`, `rnprobe`, Sideband, Nomadnet, …) to talk to the running
daemon, your user has to be in the `leviculum` group:

```sh
sudo usermod -aG leviculum "$USER"
# log out and back in, or `newgrp leviculum` for this shell only
```

Verify the installation:

```sh
lnsd --version          # e.g. 0.7.0-nightly.20260419-5a5df20
lnstest  --version
systemctl is-active lnsd
```

`is-active` should print `active`. If it prints `failed`, jump to
[Troubleshooting](#troubleshooting).

## Minimum-viable config

The default `/etc/reticulum/config` is conservative: it brings up a
single `AutoInterface` for local LAN peers, with transport routing
disabled. That's enough to talk to other Reticulum nodes on the same
LAN, but it does not connect you to the wider mesh.

A reasonable beta-tester config has two interfaces: one for the LAN, one
TCP uplink to a public entrypoint. Edit `/etc/reticulum/config` to:

```ini
[reticulum]
  # Pass announces and serve paths for other peers. Leave off if your
  # machine is mobile or sleeps a lot.
  enable_transport = Yes

  # Required for `lnstest diag`, `rnstatus`, Sideband etc. to attach to
  # this daemon. The default config already sets this.
  share_instance = Yes

[interfaces]

  # 1. Local mesh: discovers and talks to every other Reticulum node
  # on the same broadcast domain. No router/DHCP needed. Multicast
  # has to reach the link (most home LANs do; corporate Wi-Fi often
  # does not).
  [[Default Interface]]
    type = AutoInterface
    enabled = Yes

  # 2. TCP uplink to a public entrypoint. Pick a node from the
  # community directory: https://directory.rns.recipes/  (entrypoints
  # rotate; for redundancy add two or three, and see the Reticulum
  # manual's "Bootstrapping Connectivity" section for the
  # discover_interfaces auto-peering option). Example below: the
  # RNS TCP Node Germany 002 entry.
  [[RNS TCP Node Germany 002]]
    type = TCPClientInterface
    enabled = Yes
    target_host = 193.26.158.230
    target_port = 4965
```

Then restart the daemon so it picks up the new config:

```sh
sudo systemctl restart lnsd
```

`lnstest diag` (below) is the easiest way to confirm both interfaces came
up.

## Start the daemon

The systemd unit handles this for you on install. The relevant commands:

```sh
sudo systemctl start lnsd      # or restart
sudo systemctl stop lnsd
sudo systemctl status lnsd
journalctl -u lnsd -f          # live log tail
journalctl -u lnsd --since '10 min ago'
```

Logs go to the journal. Increase verbosity by editing the unit's
`ExecStart` to add `-v` (debug) or `-vv` (trace), then
`sudo systemctl daemon-reload && sudo systemctl restart lnsd`. The
`RUST_LOG` environment variable also works (see `lnsd --help`).

To run `lnsd` by hand without systemd (useful for ad-hoc debugging):

```sh
sudo systemctl stop lnsd
sudo -u leviculum /usr/bin/lnsd -v --config /etc/reticulum
```

## Check it's working

Three commands. Run them as a user that is in the `leviculum` group.

### 1. `lnstest diag`

This is the main health-check. It connects to the running daemon over
the shared-instance socket and renders a single-file diagnostic bundle:

```sh
lnstest diag --config /etc/reticulum
```

A healthy bundle looks roughly like this (your `transport id`, paths,
and byte counters will differ):

```
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
interfaces (2):
  - AutoInterface[Default Interface]  type=AutoInterface status=up rxb=482 txb=917 peers=2
  - tcp_client_0  type=TCPClientInterface status=up rxb=14211 txb=8332

raw:
{ ... full JSON dump of the same data, one object per interface ... }

## path_table
known paths: 7
[ ... JSON array of {hash, interface, hops, expires, ...} ... ]

## link_count
relayed links: 0

## link_table
links (0):

raw:
[ ... JSON array of the links this node TERMINATES; note that
  `link_count` above counts a different table, the links it RELAYS.
  This section is a Leviculum extension and shows <unavailable>
  against a Python rnsd ... ]

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

What to look at first:

- **`status=up` on every interface** in the `interface_stats` section.
  An interface that came up but lost its medium reports `status=down`.
- **Non-zero `rxb` / `txb`** on the interfaces you expect traffic on
  (`AutoInterface` once any other Reticulum node is on the same LAN,
  the TCP uplink `tcp_client_N` as soon as it connects).
- **`peers=…`** on the `AutoInterface` line: how many other Reticulum
  nodes are visible on the LAN.
- **`known paths: N`** with N > 0 once announces have crossed the
  mesh. Brand-new daemons that haven't heard any announces yet show
  `known paths: 0` for the first few seconds — that's normal.
- **`transport id`** is your node's identity (the public half). It is
  safe to share; the private half lives in
  `/etc/reticulum/storage/transport_identity` and is never included
  in `lnstest diag` output.

### 2. `lnstest selftest --help`

Sanity-checks that `lnstest` itself is installed and runnable:

```sh
lnstest selftest --help
```

The actual `lnstest selftest` exercise needs one or two relay nodes you
control. The full command and options are in `lnstest selftest --help`.

### 3. `rnstatus` (optional — Python tools)

The `.deb` does **not** install Python Reticulum. If you want `rnstatus`
/ `rnpath` / `rnprobe` / Sideband, install it in its own environment.
Debian 12+ and Ubuntu 24.04+ refuse `pip install` into the system
Python with an `externally-managed-environment` error (PEP 668), so use
`pipx` (or a venv):

```sh
sudo apt install pipx
pipx install rns
rnstatus
```

With a plain virtual environment instead:

```sh
sudo apt install python3-venv
python3 -m venv ~/.rns-venv
~/.rns-venv/bin/pip install rns
~/.rns-venv/bin/rnstatus
```

Python tools auto-detect `/etc/reticulum/config` and connect to the
running `lnsd` through the same shared-instance socket. No extra flags
are needed. (The native `lnstatus` from the `.deb` covers the same
ground as `rnstatus`; the Python install is only needed for `rnpath`,
`rnprobe`, Sideband, and friends.)

## Connect to the wider mesh

With the config above, two things happen as soon as `lnsd` starts:

1. **Announcing.** Your node sends an announce for its probe destination
   on every enabled interface. Other transport-enabled nodes pass
   that announce on, so within seconds your node is visible to peers
   on the LAN and within a few minutes to peers reachable through
   the TCP uplink.
2. **Learning paths.** When other nodes announce, your daemon stores
   a path to each announced destination (destination hash, the
   interface it was heard on, hop count, expiry). `lnstest diag`'s
   `known paths: N` is that table's size.

When you want to talk to a specific destination (e.g. send a file with
`lncp`), the daemon either has a path already (immediate) or requests
one (a path-request packet, a few seconds, then immediate). You don't
have to do anything to make path discovery happen — it runs whenever
the daemon is up.

For the protocol-level picture, read [Bootstrapping
Connectivity](https://reticulum.network/manual/gettingstartedfast.html#bootstrapping-connectivity)
in the upstream Reticulum manual.

## Troubleshooting

### lnsd will not start

```sh
systemctl status lnsd
journalctl -u lnsd --since '10 min ago' | tail -50
```

Common causes:

- **Config not parsed.** Look for a "Failed to parse config" line in
  the journal. `lnstest diag --no-rpc` shows the parse status without
  needing the daemon up:
  ```
  config file: present but FAILED to parse: <details>
  ```
- **Abstract socket already in use.** Another `lnsd` or `rnsd` is
  running under the same `instance_name`. Stop it
  (`sudo systemctl stop lnsd` then `pkill -f rnsd` if applicable),
  or set a unique `instance_name` in your config.
- **Permission on the storage directory.** The `leviculum` user has
  to be able to write `/etc/reticulum/storage/`. The `.deb` sets the
  permissions correctly on install; a manual `chown` to `root:root`
  breaks the daemon. Fix:
  ```sh
  sudo chown -R leviculum:leviculum /etc/reticulum
  sudo chmod 2775 /etc/reticulum
  ```

### No peers found / `known paths: 0`

Check `lnstest diag`'s `interface_stats` section:

- **`AutoInterface` shows `peers: 0` and `rxb: 0`** — multicast isn't
  reaching the link. Likely causes: corporate Wi-Fi (multicast blocked);
  a Linux bridge or container network without multicast forwarding;
  no other Reticulum node on the segment.
- **`TCPClientInterface` shows `status=up` but `rxb: 0`** — TCP
  connected but the remote isn't sending anything, which usually means
  the remote is up but has no transport peers itself, or the
  entrypoint has been retired. Try a different entrypoint, or rely on
  AutoInterface + a TCP uplink to a known-good node you control.
- **`TCPClientInterface` not listed at all** — the daemon hasn't
  connected yet (look for `Establishing TCP connection` lines in
  `journalctl -u lnsd`) or DNS for the target host doesn't resolve.

Give it ~30 seconds after starting `lnsd` before concluding there's a
problem — the first round of announces and the initial TCP connect
take a moment.

### Native and Python tools cannot reach lnsd

Symptom: in the daemon-view section, `lnstest diag` shows either
`cannot derive RPC authkey: …/storage/transport_identity: Permission
denied` followed by `(daemon queries skipped)` (your user cannot read
the identity file, almost always a missing group membership), or
`<unavailable: …>` on the individual queries (the daemon is down or
you targeted the wrong instance). `rnstatus` errors with "Reticulum is
not running".

- Confirm your user is in the `leviculum` group:
  ```sh
  id | tr , '\n' | grep leviculum
  ```
  If not, `sudo usermod -aG leviculum "$USER"` and log out / back in.
- Confirm the daemon really is up and has `share_instance = Yes`:
  ```sh
  systemctl is-active lnsd
  grep -i share_instance /etc/reticulum/config
  ```
- Confirm both client and daemon are using the same config directory.
  The client defaults to `/etc/reticulum` if it exists, then
  `~/.config/reticulum`, then `~/.reticulum` — the full resolution
  order is in [Installation](guide/installation.md). `lnstest diag
  --config /etc/reticulum` is explicit.

### Submitting a bug report

Run `lnstest diag` and attach its output to your report:

```sh
lnstest diag --config /etc/reticulum --output /tmp/lnstest-diag.txt
```

The bundle is plain UTF-8 text, designed to be safe to attach: IFAC
`passphrase` and `networkname` are redacted before serialisation; the
node identity private key is never read into the bundle (only its
SHA-256 is used, internally, to derive the shared-instance RPC
authkey). The bundle does contain your node's hostnames, configured
TCP targets, byte counters, and known-destinations table — review it
once before posting to a public tracker if your topology is sensitive.

If `lnsd` is in a structured event-log run
(`LEVICULUM_EVENT_LOG=/var/log/lnsd-events.log` in the service unit's
`Environment=`), include the tail of that file too:

```sh
lnstest diag --event-log /var/log/lnsd-events.log \
         --output /tmp/lnstest-diag.txt
```

Otherwise the bundle already points the reviewer at `journalctl -u
lnsd`, which is enough.

## See also

- `lnsd --help`, `lnstest --help`, `lncp --help`, `lnstatus --help` for
  the full command and option reference; the `.deb` also installs a man
  page for each (`man lnsd`, …).
- [Configuration](guide/configuration.md) for the format reference.
- [Installation](guide/installation.md) for the source-build path.
- The upstream [Reticulum Manual](https://reticulum.network/manual/)
  for the protocol itself.
