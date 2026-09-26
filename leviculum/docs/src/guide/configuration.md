# Configuration

`lnsd` reads the same INI-style configuration file as Python Reticulum
(`rnsd`). The format is a drop-in: a config that `rnsd` accepts, `lnsd`
accepts, and the two share the shared-instance IPC socket so client
tools (`rnstatus`, `rncp`, `lnstest diag`, Sideband, Nomadnet) attach to
either daemon without changes. Keys `lnsd` does not implement are
tolerated, not rejected — an unknown key never makes `lnsd` refuse a
config a current `rnsd` would load (`ini_config.rs:303-308`).

## File location and lookup order

Pass an explicit config directory with `--config DIR` (`lnsd.rs`,
`-c/--config`). With no flag, `lnsd` resolves the directory using the
same order as Python Reticulum (`config.rs:668-685`):

1. `/etc/reticulum` — if `/etc/reticulum/config` exists
2. `$HOME/.config/reticulum` — if that directory's `config` exists
3. `$HOME/.reticulum` — fallback, used even if absent

The config *file* is always named `config` inside that directory
(`config.rs:687-689`). The storage directory defaults to
`<config_dir>/storage` and can be overridden with `--storage`
(`lnsd.rs`, `-s/--storage`).

This order is why the Debian package can install a system-wide config
under `/etc/reticulum` and have Python clients connect to the live
daemon with no extra flags (`config.rs:674-678`).

## INI vs TOML detection

`lnsd` accepts both the Python INI format and native TOML. Detection is
by content, not just extension (`config.rs:630-656`):

- An explicit `.toml` extension forces TOML.
- A file containing `[[` (the ConfigObj subsection marker Python uses
  for interfaces) is parsed as INI.
- Otherwise TOML is tried first, then INI as a fallback.

In practice your `config` file uses the Python INI form shown
throughout this page. Boolean values accept `Yes`, `yes`, `True`,
`true`, `1`, `on` (and their false counterparts); anything else is read
as `false` (`ini_config.rs:619-630`).

## The `[reticulum]` section

Core daemon settings. Every key below is parsed in
`ini_config.rs:215-310`; defaults come from `config.rs:199-224`.

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `enable_transport` | bool | `true` | Route announces and serve paths for other peers. `lnsd` defaults this to `true` (it is a daemon); the Python *library* default is `false`. (`config.rs:27-28`, `202`) |
| `use_implicit_proof` | bool | `true` | Use implicit proof for link identification. (`config.rs:30-31`, `203`; `ini_config.rs:265-267`) |
| `share_instance` | bool | `false` | Listen on the abstract Unix socket `\0rns/<instance_name>` for local clients. Required for `lnstest diag`, `rnstatus`, Sideband etc. to attach. (`config.rs:37-40`, `205`; key `share_instance` → `shared_instance`, `ini_config.rs:228-230`) |
| `instance_name` | string | `default` | Names the shared-instance socket: `\0rns/<instance_name>`. Use a unique name to run two daemons side by side. (`config.rs:41-44`, `206`; `ini_config.rs:231-233`) |
| `shared_instance_type` | `unix`/`tcp` | unset | Parsed for `rnsd` compatibility. Only `tcp`/`unix` are stored; `tcp` clears `shared_instance_socket` (tcp disables AF_UNIX upstream). `lnsd` currently serves only the abstract AF_UNIX socket. (`config.rs:45-52`; `ini_config.rs:234-243`, `179-181`) |
| `shared_instance_socket` | path | unset | Explicit AF_UNIX socket path (RNS 1.3.x). Parsed for compatibility; cleared when `shared_instance_type = tcp`. (`config.rs:53-58`; `ini_config.rs:244-246`) |
| `respond_to_probes` | bool | `false` | Answer `rnprobe` requests by signing a proof for each probe packet. (`config.rs:54-60`, `146`; `ini_config.rs:251-253`) |
| `remote_management_enabled` | bool | `false` | Enable remote management. (`config.rs:61-63`, `147`; `ini_config.rs:263-267`) |
| `storage_path` | path | unset | Storage path, relative to the config dir or absolute. (`config.rs:64-66`, `148`) |
| `flush_interval` | u64 (sec) | `3600` | Seconds between periodic storage flushes. Crash protection only — normal shutdown always flushes. (`config.rs:67-73`, `149`; `ini_config.rs:275-279`) |
| `control_channel_capacity` | usize | `256` | Capacity of the lossless control-plane event channel (announces, paths, link/resource lifecycle). Raise on servers under heavy announce load. (`config.rs:74-82`, `150`) |
| `data_channel_capacity` | usize | `128` | Capacity of the droppable data-plane event channel; full means normal backpressure (silent drop). (`config.rs:83-90`, `151`) |
| `keepalive_interval` | u64 (sec) | unset | Override link keepalive interval. When set, every link uses this interval and the stale-link timeout scales with it (stale after twice the keepalive). Local timing only, no wire change. Useful for slow links. (`config.rs:91-98`, `152`; `ini_config.rs:280-287`) |

`storage_path`, `control_channel_capacity`, and
`data_channel_capacity` are read from TOML only; they have no INI key in
`apply_reticulum_key` (`ini_config.rs:215-310`) and are best set in a
TOML config or left at their defaults.
`storage_path` is also settable from the command line via `lnsd --storage`.

`flush_interval` and `keepalive_interval` are Leviculum tuning
extensions — Python Reticulum ignores them. Battery-powered or SD-card
deployments may want a longer `flush_interval`; slow links benefit from
a fixed `keepalive_interval`:

```ini
[reticulum]
  # Seconds between periodic storage flushes (crash protection only,
  # normal shutdown always flushes). Default: 3600.
  flush_interval = 3600

  # Link keepalive interval in seconds. When set, every link uses this
  # interval instead of the RTT-derived default. Default: unset.
  keepalive_interval = 360
```

## The `[interfaces]` section

Interfaces are ConfigObj subsections under `[interfaces]`, each named in
double brackets `[[Name]]`. The name is free-form; the `type` key
selects the interface implementation. Twelve types pass the
supported-type filter (`interface_type` (`ini_config.rs:192-216`)):

`TCPServerInterface`, `TCPClientInterface`, `UDPInterface`,
`AutoInterface`, `RNodeInterface`, `RNodeMultiInterface`,
`SerialInterface`, `PipeInterface`, `KISSInterface`,
`AX25KISSInterface`, `I2PInterface`, `BLEInterface`.

`BackboneInterface` and `BackboneClientInterface` are accepted too:
they are wire-identical to TCP and are mapped onto the TCP interface at
parse time, as Python does (`normalize_backbone_interface`
(`ini_config.rs:587-617`)). An interface of any other type is skipped
with a log line (`tracing::warn` (`ini_config.rs:206-214`)), not an
error.

The per-type tables below cover the six types most deployments use. All
interface keys are parsed in `apply_interface_key`
(`ini_config.rs:364-542`); the struct they land in, with its defaults,
is `InterfaceConfig` (`config.rs:228-499`).

### Keys common to every interface

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `type` | string | (required) | Interface type, one of the eleven above. (`type` (`ini_config.rs:366`)) |
| `enabled` | bool | `true` | Bring this interface up; the legacy spelling `interface_enabled` is honoured too. (`enabled` (`ini_config.rs:370`); `InterfaceConfig::enabled` (`config.rs:239-241`)) |
| `outgoing` | bool | `true` | Allow sending outgoing packets. (`outgoing` (`ini_config.rs:371`); `InterfaceConfig::outgoing` (`config.rs:249-251`)) |
| `bitrate` | u64 (bps) | per type | Override the interface's own bitrate figure, which feeds announce bandwidth capping and timing. Values below `MINIMUM_BITRATE` (`constants.rs:308-311`), 5 bps, are ignored. (`bitrate` (`ini_config.rs:428-434`); `InterfaceConfig::bitrate` (`config.rs:252-257`)) |
| `buffer_size` | usize | per type | Channel buffer size. (`buffer_size` (`ini_config.rs:461`); `InterfaceConfig::buffer_size` (`config.rs:372-374`)) |

### TCP server (`TCPServerInterface`)

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `listen_ip` | string | unset | Address to bind. (`listen_ip` (`ini_config.rs:378`)) |
| `listen_port` | u16 | unset | Port to listen on. (`listen_port` (`ini_config.rs:379`)) |

```ini
[interfaces]
  [[Loopback TCP]]
    type = TCPServerInterface
    enabled = Yes
    listen_ip = 127.0.0.1
    listen_port = 45999
```

### TCP client (`TCPClientInterface`)

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `target_host` | string | unset | Remote host to connect to. (`target_host` (`ini_config.rs:380`)) |
| `target_port` | u16 | unset | Remote port. (`target_port` (`ini_config.rs:381`)) |
| `reconnect_interval` | u64 (sec) | `5` | Delay between reconnect attempts. (`reconnect_interval` (`ini_config.rs:462`); `InterfaceConfig::reconnect_interval_secs` (`config.rs:375-376`)) |
| `max_reconnect_tries` | u64 | unlimited | Give up after this many attempts; unset means never. (`max_reconnect_tries` (`ini_config.rs:463`); `InterfaceConfig::max_reconnect_tries` (`config.rs:377-378`)) |

```ini
[interfaces]
  [[RNS TCP Node Germany 002]]
    type = TCPClientInterface
    enabled = Yes
    target_host = 193.26.158.230
    target_port = 4965
```

### UDP (`UDPInterface`)

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `listen_ip` | string | unset | Local bind address. (`listen_ip` (`ini_config.rs:378`)) |
| `listen_port` | u16 | unset | Local bind port. (`listen_port` (`ini_config.rs:379`)) |
| `forward_ip` | string | unset | Broadcast/forward address or hostname. Names are resolved at runtime and re-resolved periodically; a resolution failure is a logged interface error, not a config error. (`forward_ip` (`ini_config.rs:389`); `InterfaceConfig::forward_ip` (`config.rs:311-321`)) |
| `forward_port` | u16 | unset | Broadcast/forward port. (`forward_port` (`ini_config.rs:390`)) |

### AutoInterface (`AutoInterface`)

Discovers other Reticulum nodes on the same broadcast domain via
multicast. No router or DHCP needed; the link must carry multicast.

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `group_id` | string | unset | Multicast group identifier; isolate co-located meshes by setting different IDs. (`group_id` (`ini_config.rs:475`); `InterfaceConfig::group_id` (`config.rs:380-382`)) |
| `discovery_scope` | string | unset | Multicast scope: `link`, `admin`, `site`, `organisation`, `global`. (`discovery_scope` (`ini_config.rs:476`); `InterfaceConfig::discovery_scope` (`config.rs:383-384`)) |
| `discovery_port` | u16 | `29716` | Discovery (announce) port. (`discovery_port` (`ini_config.rs:477`); `InterfaceConfig::discovery_port` (`config.rs:385-386`)) |
| `data_port` | u16 | `42671` | Data port. (`data_port` (`ini_config.rs:478`); `InterfaceConfig::data_port` (`config.rs:387-388`)) |
| `devices` | string (CSV) | unset | Whitelist of NIC names to use. (`devices` (`ini_config.rs:479`); `InterfaceConfig::devices` (`config.rs:389-390`)) |
| `ignored_devices` | string (CSV) | unset | Blacklist of NIC names to skip. (`ignored_devices` (`ini_config.rs:480`); `InterfaceConfig::ignored_devices` (`config.rs:391-392`)) |
| `multicast_loopback` | bool | unset (inherits `true`) | Multicast loopback (`IPV6_MULTICAST_LOOP`), the carrier self-echo mechanism. Unset inherits the default `true`, matching Python-RNS; set `no` to opt out. (`multicast_loopback` (`ini_config.rs:481`); `InterfaceConfig::multicast_loopback` (`config.rs:393-396`)) |

### BLE (`BLEInterface`)

Joins the Columba BLE mesh (the `ble-reticulum` protocol, v2.2 wire
format with the v0.3.0 capability record) as a dual-role BlueZ node: it
advertises and serves the Columba GATT layout like an LNode board does,
and it scans for and connects to nearby peers under the same
connection-direction rule the boards and phones apply. One section is
one Reticulum interface — a single broadcast domain across all live BLE
links. Requires a BlueZ (`bluetoothd`) host with a BLE-capable adapter;
if the adapter is missing or powered off at startup the interface keeps
retrying rather than failing the daemon.

The interface is off unless a `[[BLE Interface]]` section exists in the
config; the daemon never brings BLE up on its own. The advertised name
is derived from the daemon identity as `LN-<hex8>` exactly like the
firmware's, so scanner listings show lnsd and boards the same way.

Key names follow the reference `ble-reticulum` package where its options
map onto this implementation:

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `device` | string | default adapter | BlueZ adapter to use, e.g. `hci0`. (`device` (`ini_config.rs:377`)) |
| `max_connections` | usize | `4` | Simultaneous BLE link cap, both GATT roles counted together. The default is the firmware's `MAX_LINKS` (4), not the reference's 7: 3-4 links is the protocol's reliable ceiling. (`max_connections` (`ini_config.rs:541`); `InterfaceConfig::max_connections` (`config.rs:510-513`)) |
| `min_rssi` | i16 (dBm) | `-85` | Sightings weaker than this are not dialled. (`min_rssi` (`ini_config.rs:542`); `InterfaceConfig::min_rssi` (`config.rs:514-516`)) |
| `discovery_interval` | f64 (sec) | `5` | Pause between the 2-second BLE scan windows. (`discovery_interval` (`ini_config.rs:543`); `InterfaceConfig::discovery_interval` (`config.rs:517-519`)) |
| `enable_central` | bool | `true` | Run the scanning + dialling central role. (`enable_central` (`ini_config.rs:544`); `InterfaceConfig::enable_central` (`config.rs:520-522`)) |
| `enable_peripheral` | bool | `true` | Run the advertising + GATT-server peripheral role. Disabling both roles is a config error. (`enable_peripheral` (`ini_config.rs:545`); `InterfaceConfig::enable_peripheral` (`config.rs:523-525`)) |

```ini
[interfaces]
  [[BLE Interface]]
    type = BLEInterface
    enabled = yes
    # device = hci0
    # max_connections = 4
    # min_rssi = -85
```

### RNode and Serial (`RNodeInterface`, `SerialInterface`)

`RNodeInterface` drives an RNode LoRa modem; `SerialInterface` is a raw
serial HDLC link. They share the serial-port and LoRa keys.

Divergence from Python: there, only `RNodeInterface` honours the LoRa
keys (`frequency`, `bandwidth`, `spreadingfactor`, `codingrate`,
`txpower`) and `SerialInterface` reads port settings only. Leviculum's
`SerialInterface` honours them too and configures the attached LNode's
radio over the serial port — the LNode frames HDLC, so it cannot be
driven by the KISS-framed `RNodeInterface`.

Serial keys:

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `port` | string | unset | Serial device path, e.g. `/dev/ttyACM0`. (`port` (`ini_config.rs:391`); `InterfaceConfig::port` (`config.rs:327-328`)) |
| `speed` / `baudrate` | u32 | unset | Serial baud rate (either spelling). (`speed` (`ini_config.rs:392`); `InterfaceConfig::speed` (`config.rs:329-330`)) |
| `databits` | u8 | unset | Data bits. (`databits` (`ini_config.rs:393`); `InterfaceConfig::databits` (`config.rs:331-332`)) |
| `parity` | string | unset | `none`, `even`, or `odd`. (`parity` (`ini_config.rs:394`); `InterfaceConfig::parity` (`config.rs:333-334`)) |
| `stopbits` | u8 | unset | Stop bits. (`stopbits` (`ini_config.rs:395`); `InterfaceConfig::stopbits` (`config.rs:335-336`)) |

LoRa keys, derived from source — the meanings below describe the radio
parameters the interface configures; the fields sit together in the
RNode block of `InterfaceConfig` (`InterfaceConfig::frequency`
(`config.rs:446-477`)):

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `frequency` | u64 (Hz) | unset | LoRa centre frequency. (`frequency` (`ini_config.rs:464`); `InterfaceConfig::frequency` (`config.rs:446-448`)) |
| `bandwidth` | u32 (Hz) | unset | LoRa bandwidth. (`bandwidth` (`ini_config.rs:465`); `InterfaceConfig::bandwidth` (`config.rs:449-450`)) |
| `spreadingfactor` / `spreading_factor` | u8 | unset | LoRa spreading factor (either spelling). (`spreadingfactor` (`ini_config.rs:466`); `InterfaceConfig::spreading_factor` (`config.rs:451-452`)) |
| `codingrate` / `coding_rate` | u8 | unset | LoRa coding rate (either spelling). (`codingrate` (`ini_config.rs:467`); `InterfaceConfig::coding_rate` (`config.rs:453-454`)) |
| `txpower` / `tx_power` | i8 (dBm) | unset (resolves to the board maximum, 22 dBm) | Transmit power (either spelling). Unset asks the board for its maximum — a board that can do less clamps and says so — rather than the 0 dBm (1 mW) Python-Reticulum resolves it to, which has no symptom at the node. An explicit `txpower = 0` still means 0. Above roughly 7 dBi of antenna gain, 22 dBm conducted exceeds the EU 27 dBm ERP allowance and has to be set down. (`txpower` (`ini_config.rs:468`); `InterfaceConfig::tx_power` (`config.rs:455-456`); `resolve_tx_power` (`rnode.rs:693`); [deviation](../concepts/python-rns-compatibility.md)) |
| `flow_control` | bool | unset | Wait for the RNode's `CMD_READY` before the next TX. (`flow_control` (`ini_config.rs:482`); `InterfaceConfig::flow_control` (`config.rs:473-474`)) |
| `airtime_limit_short` | f64 (%) | unset | Short-term airtime cap, percent (0.0–100.0). (`airtime_limit_short` (`ini_config.rs:483`); `InterfaceConfig::airtime_limit_short` (`config.rs:475-476`)) |
| `airtime_limit_long` | f64 (%) | unset | Long-term airtime cap, percent (0.0–100.0). (`airtime_limit_long` (`ini_config.rs:484`); `InterfaceConfig::airtime_limit_long` (`config.rs:477-478`)) |
| `csma_enabled` | bool | unset | Carried in the LNode radio-config frame and reported back, but current firmware no longer obeys it: LoRa channel access (pre-TX jitter plus CAD listen-before-talk) is always on, matching the RNode firmware, which offers no CSMA disable either. Only firmware older than the change still honours the flag. (`csma_enabled` (`ini_config.rs:485`); `InterfaceConfig::csma_enabled` (`config.rs:479-480`)) |
| `preamble_symbols` | u16 (symbols) | unset (derived from the PHY) | LoRa preamble length pushed to LNode firmware in the radio-config frame (`SerialInterface` only). Unset derives what an RNode peer programs for the same PHY — 24 symbols at SF7/BW125, the 18-symbol floor from SF8 down — so a mixed pair agrees on the wire; set it only to pin a value against a non-conforming peer. A pin above roughly 20 symbols / 164 ms on air is warned about at startup and not refused: SX127x receivers (every RNode) were measured going deaf above that, losing every frame from the interface silently and one-way, while an SX126x peer copes ([Codeberg #315](https://codeberg.org/Lew_Palm/leviculum/issues/315)). Not the same key as the KISS `preamble` (TX delay in ms), which never reaches a LoRa modem. (`preamble_symbols` (`ini_config.rs:473`); `InterfaceConfig::preamble_symbols` (`config.rs:457-469`); `derive_preamble_symbols` (`rnode.rs:839`); `preamble_ceiling_warning` (`interfaces/serial.rs`)) |

**Test-only:** `test_drop_direct_ingress` (bool, default off) emulates
out-of-range placement on a co-located rig: the interface drops every
received frame whose wire hops byte (`raw[1]`) is 0 — frames heard
directly from their originator — before they reach the transport, while
relayed copies (hops ≥ 1) pass. Two endpoints with this knob on one
bench are mutually deaf but both hear a relay, giving the A–B–C
repeater topology without attenuators; the consumer is the 3-node relay
hardware scenario (periculum `hardware/lora_3node_relay.toml`).
Python-RNS has no such option — frames are dropped locally on ingress
and nothing on the air changes, so this is a test-harness affordance,
not a wire or semantic deviation. Supported on `RNodeInterface` and
`SerialInterface`; incompatible with IFAC (which prepends material
before the flags byte), and that combination is refused at startup.

### IFAC (Interface Access Codes)

IFAC keys apply to any interface and authenticate / isolate a virtual
network on the link. They are common to all interface types:

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `networkname` / `network_name` | string | unset | Network name for IFAC (either spelling). (`networkname` (`ini_config.rs:519`); `InterfaceConfig::networkname` (`config.rs:438-440`)) |
| `passphrase` / `pass_phrase` | string | unset | IFAC passphrase (either spelling). (`passphrase` (`ini_config.rs:520`); `InterfaceConfig::passphrase` (`config.rs:441-442`)) |
| `ifac_size` | usize (bits) | unset | IFAC size, specified in bits in the file and stored as bytes (`bits / 8`). Values below 8 bits are dropped, as Python drops them, so the interface falls back to its per-type default. (`ifac_size` (`ini_config.rs:527-533`); `InterfaceConfig::ifac_size` (`config.rs:443-444`)) |

`networkname` and `passphrase` are secrets: `lnstest diag` redacts them
before serialising a bundle (see the [`lnstest diag`](lnstest.md#diag) section).

## Example configurations

### Simple AutoInterface node

A node that talks to other Reticulum peers on the same LAN, no transport
routing:

```ini
[reticulum]
  enable_transport = No
  share_instance = Yes

[interfaces]
  [[Default Interface]]
    type = AutoInterface
    enabled = Yes
```

### TCP-server transport node

A routing entrypoint that accepts inbound TCP peers and bridges them
with the local LAN:

```ini
[reticulum]
  enable_transport = Yes
  share_instance = Yes
  instance_name = entrypoint

[interfaces]
  [[Public TCP]]
    type = TCPServerInterface
    enabled = Yes
    listen_ip = 0.0.0.0
    listen_port = 4965

  [[Local LAN]]
    type = AutoInterface
    enabled = Yes
```

### LoRa RNode node

A node on a LoRa RNode modem (radio values below are an EU 868 MHz
example; set them for your region and hardware):

```ini
[reticulum]
  enable_transport = Yes
  share_instance = Yes

[interfaces]
  [[LoRa RNode]]
    type = RNodeInterface
    enabled = Yes
    port = /dev/ttyACM0
    frequency = 867200000
    bandwidth = 125000
    spreadingfactor = 8
    codingrate = 5
    txpower = 14
```

See the upstream
[Reticulum Manual](https://reticulum.network/manual/) for the
protocol-level meaning of the radio and IFAC parameters.
