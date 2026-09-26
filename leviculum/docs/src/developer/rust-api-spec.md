# Rust API Reference

This chapter is a reference for the key entry points and core value types of the
Leviculum Rust API, organized by type. Each signature carries a `file:line`
citation to the source as of this writing. It is deliberately *not* exhaustive:
the complete per-type method list is generated rustdoc (see
[Full rustdoc](#full-rustdoc) at the end). Use this chapter to orient, then
rustdoc for the long tail.

The hands-on introduction is the [tutorial](rust-api-tutorial.md); the layer
overview is [Choosing a layer](choosing-a-layer.md).

All `leviculum-std` types are re-exported from the crate root
(`leviculum-std/src/lib.rs:35-57`), so `use leviculum_std::{NodeEvent, LinkHandle,
…}` works without naming submodules.

## `leviculum-std` (std / tokio)

### `Reticulum`

The configuration-driven entry point, wrapping a `ReticulumNode`. Defined at
`leviculum-std/src/reticulum.rs:13`. Use this when your node is described by a
`Config` (an INI file or a programmatic `Config`); use `ReticulumNodeBuilder`
when you assemble interfaces in code.

| Signature | Purpose |
|-----------|---------|
| `fn new() -> Result<Self>` — `reticulum.rs:22` | Build from the default config path, or defaults if absent |
| `fn with_config(config: Config) -> Result<Self>` — `reticulum.rs:37` | Build from an explicit `Config` |
| `fn with_config_daemon(config: Config) -> Result<Self>` — `reticulum.rs:55` | Like `with_config` but with no application event channel (daemon mode); `take_event_receiver()` then returns `None` |
| `async fn start(&mut self) -> Result<()>` — `reticulum.rs:67` | Spawn the event loop |
| `async fn stop(&mut self) -> Result<()>` — `reticulum.rs:73` | Stop and persist |
| `fn is_running(&self) -> bool` — `reticulum.rs:80` | Whether the loop is running |
| `fn config(&self) -> &Config` — `reticulum.rs:85` | Borrow the active config |
| `fn take_event_receiver(&mut self) -> Option<EventReceiver>` — `reticulum.rs:110` | Take the event stream, once |

### `ReticulumNodeBuilder`

The programmatic builder. Defined at `leviculum-std/src/driver/builder.rs:39`;
re-exported as `leviculum_std::ReticulumNodeBuilder`. Each setter consumes and
returns `self`.

| Signature | Purpose |
|-----------|---------|
| `fn new() -> Self` — `builder.rs:93` | Builder with defaults |
| `fn identity(self, identity: Identity) -> Self` — `builder.rs:207` | Pin an explicit identity (else one is generated/persisted) |
| `fn add_tcp_client(self, addr: SocketAddr) -> Self` — `builder.rs:258` | Connect outward to a Reticulum node |
| `fn add_tcp_server(self, addr: SocketAddr) -> Self` — `builder.rs:311` | Listen for inbound connections |
| `fn add_udp_interface(self, listen: SocketAddr, forward: SocketAddr) -> Self` — `builder.rs:372` | One datagram per packet |
| `fn add_rnode_interface(self, port: String, frequency: u64, bandwidth: u32, spreading_factor: u8, coding_rate: u8, tx_power: i8) -> Self` — `builder.rs:412` | LoRa interface; required radio settings |
| `fn add_serial_interface(self, port: String, speed: u32, databits: u8, parity: String, stopbits: u8) -> Self` — `builder.rs:471` | KISS over raw serial |
| `fn add_auto_interface(self) -> Self` — `builder.rs:584` | IPv6 multicast LAN discovery |
| `fn enable_transport(self, enabled: bool) -> Self` — `builder.rs:627` | Act as a relay/forwarder |
| `fn config(self, config: Config) -> Self` — `builder.rs:232` | Use a pre-loaded `Config` |
| `fn config_file(self, path: PathBuf) -> Self` — `builder.rs:242` | Load an INI config file |
| `fn storage_path(self, path: PathBuf) -> Self` — `builder.rs:250` | Identity / known-destinations / ratchet store dir |
| `fn connect_to_shared_instance(self, name: impl Into<String>) -> Self` — `builder.rs:668` | Attach to a running `lnsd`/`rnsd` instead of bringing up own interfaces |
| `fn without_events(self) -> Self` — `builder.rs:199` | Daemon mode: no application event channel |
| `async fn build(self) -> Result<ReticulumNode, Error>` — `builder.rs:963` | Build the node (not yet running) |
| `fn build_sync(self) -> Result<ReticulumNode, Error>` — `builder.rs:746` | Same as `build`, outside an async context |

### `ReticulumNode`

The running node. Defined at `leviculum-std/src/driver/mod.rs:412`; re-exported
as `leviculum_std::ReticulumNode`. Selected methods:

| Signature | Purpose |
|-----------|---------|
| `async fn start(&mut self) -> Result<(), Error>` — `driver/mod.rs:575` | Spawn the event loop, bring interfaces up |
| `async fn stop(&mut self) -> Result<(), Error>` — `driver/mod.rs:1123` | Stop and flush |
| `fn is_running(&self) -> bool` — `driver/mod.rs:1176` | Loop state |
| `fn register_destination(&self, destination: Destination)` — `driver/mod.rs:1184` | Make a local destination reachable (consumes it) |
| `async fn announce_destination(&self, dest_hash: &DestinationHash, app_data: Option<&[u8]>) -> …` — `driver/mod.rs:1600` | Announce a registered destination |
| `async fn connect(&self, dest_hash: &DestinationHash, dest_signing_key: &[u8; 32]) -> Result<LinkHandle, Error>` — `driver/mod.rs:1202` | Open a link; returns a pending handle |
| `fn link_handle(&self, link_id: &LinkId) -> LinkHandle` — `driver/mod.rs:1235` | Writable handle for an already-established inbound link |
| `fn packet_sender(&self, dest_hash: &DestinationHash) -> PacketSender` — `driver/mod.rs:1853` | Single-packet send handle |
| `async fn send_single_packet(&self, …) -> …` — `driver/mod.rs:1807` | Send one unreliable datagram |
| `fn take_event_receiver(&mut self) -> Option<EventReceiver>` — `driver/mod.rs:1251` | Take the event stream, once |
| `fn identity_hash(&self) -> [u8; 16]` — `driver/mod.rs:1367` | The node's own identity hash |
| `fn has_path(&self, dest_hash: &DestinationHash) -> bool` — `driver/mod.rs:1412` | Whether a path is known |
| `fn hops_to(&self, dest_hash: &DestinationHash) -> Option<u8>` — `driver/mod.rs:1453` | Hop count to a destination |
| `async fn request_path(&self, dest_hash: &DestinationHash) -> Result<(), Error>` — `driver/mod.rs:1437` | Send a PATH_REQUEST; result arrives as `PathFound` |
| `fn get_identity(&self, dest_hash: &DestinationHash) -> Option<Identity>` — `driver/mod.rs:1421` | Identity learned from an announce (its signing key feeds `connect`) |
| `fn transport_stats(&self) -> TransportStats` — `driver/mod.rs:1547` | `rnstatus`-style counters |
| `fn is_transport_enabled(&self) -> bool` — `driver/mod.rs:1867` | Relay mode flag |

The stable, curated facade `leviculum_std::api` — `NodeBuilder` (`leviculum-std/src/api/mod.rs:60`),
`Node` (`leviculum-std/src/api/mod.rs:238`) — re-projects this surface with core internals
hidden; it is what `leviculum-ffi` wraps. Notable facade-only helpers:
`api::generate_identity()` (`api/mod.rs:35`), `api::version()` (`api/mod.rs:42`),
`api::version_string()` (`api/mod.rs:51`), and `Node::connect_with_key`
(`api/mod.rs:455`) / `Node::accept_link` (`api/mod.rs:471`).

### `LinkHandle`

Send-only async handle for a link. Defined at `leviculum-std/src/driver/stream.rs:45`;
re-exported as `leviculum_std::LinkHandle`. Incoming data is delivered via
`NodeEvent`, not on the handle.

| Signature | Purpose |
|-----------|---------|
| `fn link_id(&self) -> &LinkId` — `stream.rs:72` | The link's id |
| `fn is_closed(&self) -> bool` — `stream.rs:77` | Handle state |
| `async fn try_send(&self, data: &[u8]) -> Result<(), Error>` — `stream.rs:86` | Non-blocking send; surfaces `Busy` / `PacingDelay` |
| `async fn send(&self, data: &[u8]) -> Result<(), Error>` — `stream.rs:108` | Send, retrying pacing/busy internally |
| `async fn close(&mut self) -> Result<(), Error>` — `stream.rs:145` | Graceful close (sends LINKCLOSE) |

### `PacketSender`

Send-only async handle for single packets, the single-packet analog of
`LinkHandle`. Defined at `leviculum-std/src/driver/sender.rs:42`; re-exported as
`leviculum_std::PacketSender`.

| Signature | Purpose |
|-----------|---------|
| `fn dest_hash(&self) -> &DestinationHash` — `sender.rs:63` | The target destination |
| `async fn send(&self, data: &[u8]) -> Result<[u8; TRUNCATED_HASHBYTES], Error>` — `sender.rs:74` | Send one unreliable packet; returns the truncated packet hash. A path must already be known |

### `EventReceiver` and `NodeEvent`

`EventReceiver` is the merged event stream, defined at
`leviculum-std/src/driver/mod.rs:259`. It internally fronts a lossless control
plane and a droppable data plane (Codeberg #71), draining control first.

| Signature | Purpose |
|-----------|---------|
| `async fn recv(&mut self) -> Option<NodeEvent>` — `driver/mod.rs:273` | Next event, control plane prioritized; `None` once shut down. Cancel-safe |
| `fn try_recv(&mut self) -> Result<NodeEvent, TryRecvError>` — `driver/mod.rs:298` | Non-blocking receive |

`NodeEvent` is the event enum, defined in core at
`leviculum-core/src/node/event.rs:21` and re-exported as
`leviculum_std::NodeEvent`. It is `#[non_exhaustive]`, so always include a
catch-all arm. The variants most applications match (field names verbatim from
source):

| Variant | Fields | Source |
|---------|--------|--------|
| `AnnounceReceived` | `announce: ReceivedAnnounce, interface_index: usize` | `event.rs:24` |
| `PathFound` | `destination_hash: DestinationHash, hops: u8, interface_index: usize` | `event.rs:32` |
| `PacketReceived` | `destination: DestinationHash, data: Vec<u8>, interface_index: usize` | `event.rs:59` |
| `PacketDeliveryConfirmed` | `packet_hash: [u8; TRUNCATED_HASHBYTES]` | `event.rs:69` |
| `LinkEstablished` | `link_id: LinkId, is_initiator: bool` | `event.rs:84` |
| `MessageReceived` | `link_id: LinkId, msgtype: u16, sequence: u16, data: Vec<u8>` | `event.rs:95` |
| `LinkDataReceived` | `link_id: LinkId, data: Vec<u8>` | `event.rs:111` |
| `LinkClosed` | (see source) | `event.rs:155` |

`MessageReceived` is the channel-multiplexed (sequenced) receive path;
`LinkDataReceived` is the raw-link-packet path. The full variant list (resources,
requests/responses, identify, stale/recovered, control-plane overflow) is in
`event.rs` and in rustdoc.

### `Config`

Configuration, defined at `leviculum-std/src/config.rs:11`; re-exported as
`leviculum_std::Config`. `pub reticulum: ReticulumConfig` (`config.rs:14`) and
`pub interfaces: HashMap<String, InterfaceConfig>` (`config.rs:17`).

| Signature | Purpose |
|-----------|---------|
| `fn load<P: AsRef<Path>>(path: P) -> Result<Self>` — `config.rs:315` | Load an INI config (the `rnsd`/`lnsd` format) |
| `fn default_config_dir() -> PathBuf` — `config.rs:360` | Default config directory |
| `fn default_config_path() -> PathBuf` — `config.rs:369` | Default config file path |

## `leviculum-core` (no_std, sans-IO)

The core is the no_std engine the std layer drives. You use these types directly
only when building on `leviculum-core` — see [Embedded development](embedded.md).
All are re-exported from `leviculum-core/src/lib.rs:123-143`.

### `NodeCore<R, C, S>`

The sans-IO protocol engine, generic over an RNG `R: CryptoRngCore`, a clock
`C: Clock`, and storage `S: Storage`. Defined at
`leviculum-core/src/node/mod.rs:143`. It never performs I/O; every method that
can produce output returns a [`TickOutput`](#core-tickoutput-and-action) the
caller must dispatch.

| Signature | Purpose |
|-----------|---------|
| `fn new(identity: Identity, config: TransportConfig, proof_strategy: ProofStrategy, max_incoming_resource_size: usize, rng: R, clock: C, storage: S) -> Self` — `node/mod.rs:213` | Construct directly |
| `fn register_destination(&mut self, dest: Destination)` — `node/mod.rs:256` | Register a local destination |
| `fn announce_destination(&mut self, dest_hash: &DestinationHash, app_data: Option<&[u8]>) -> Result<TickOutput, AnnounceError>` — `node/mod.rs:410` | Build and queue an announce |
| `fn send_single_packet(&mut self, dest_hash: &DestinationHash, data: &[u8]) -> Result<([u8; TRUNCATED_HASHBYTES], TickOutput), SendError>` — `node/mod.rs:473` | Build an unreliable data packet |
| `fn connect(&mut self, dest_hash: DestinationHash, dest_signing_key: &[u8; 32]) -> (LinkId, bool, TickOutput)` — `node/link_management.rs:185` | Build a link request |
| `fn send_on_link(&mut self, link_id: &LinkId, data: &[u8]) -> Result<TickOutput, SendError>` — `node/link_management.rs:504` | Send on an established link |
| `fn close_link(&mut self, link_id: &LinkId) -> TickOutput` — `node/link_management.rs:419` | Close a link |
| `fn handle_packet(&mut self, iface: InterfaceId, data: &[u8]) -> TickOutput` — `node/mod.rs:1032` | Feed received bytes from an interface |
| `fn handle_timeout(&mut self) -> TickOutput` — `node/mod.rs:1124` | Run periodic maintenance (call at the next deadline) |
| `fn next_deadline(&self) -> Option<u64>` — `node/mod.rs:1155` | Earliest timer deadline (ms); when to call `handle_timeout` |

A node is more often built with `NodeCoreBuilder` (`node/builder.rs:40`), whose
`fn build<R, Clk, S>(self, rng: R, clock: Clk, storage: S) -> NodeCore<R, Clk, S>`
(`node/builder.rs:215`) supplies the platform triple. Setters include
`identity`, `proof_strategy`, and `enable_transport`.

### Core `TickOutput` and `Action`

`TickOutput` is what every core method returns. Defined at
`leviculum-core/src/transport.rs:138`. It is `#[must_use]` — dropping it silently
loses outbound packets and events.

| Field | Type | Source |
|-------|------|--------|
| `actions` | `Vec<Action>` — I/O for the driver to execute | `transport.rs:140` |
| `events` | `Vec<NodeEvent>` — application-visible events | `transport.rs:142` |
| `next_deadline_ms` | `Option<u64>` — when to next call `handle_timeout` | `transport.rs:145` |

`Action` is the I/O the driver performs, defined at `leviculum-core/src/transport.rs:113`:

| Variant | Fields | Source |
|---------|--------|--------|
| `SendPacket` | `iface: InterfaceId, data: Vec<u8>, peer: Option<[u8; 16]>` | `transport.rs:115` |
| `Broadcast` | `data: Vec<u8>, exclude_iface: Option<InterfaceId>` | `transport.rs:122` |

The helper `dispatch_actions(interfaces: &mut [&mut dyn Interface], actions:
Vec<Action>, ifac_configs: &BTreeMap<usize, IfacConfig>) -> DispatchResult`
(`transport.rs:211`) routes `Action`s to interfaces with broadcast-exclusion and
IFAC wrapping handled in core, so every driver gets it for free.

### Value types

`Identity` — a key pair or public-only identity. Defined at
`leviculum-core/src/identity.rs`. Re-exported as `leviculum_std::Identity`.

| Signature | Purpose |
|-----------|---------|
| `fn generate<R: CryptoRngCore>(rng: &mut R) -> Self` — `identity.rs:71` | New random identity |
| `fn from_public_key_bytes(bytes: &[u8]) -> Result<Self, IdentityError>` — `identity.rs:113` | Public-only identity |
| `fn from_private_key_bytes(bytes: &[u8]) -> Result<Self, IdentityError>` — `identity.rs:127` | From the raw 64-byte private key (Python-compatible) |
| `fn hash(&self) -> &[u8; IDENTITY_HASHBYTES]` — `identity.rs:155` | The 16-byte identity hash |
| `fn public_key_bytes(&self) -> [u8; IDENTITY_KEY_SIZE]` — `identity.rs:160` | 64 bytes: X25519 `[0..32]`, Ed25519 `[32..64]` |
| `fn has_private_keys(&self) -> bool` — `identity.rs:185` | Whether it can sign/decrypt |
| `fn sign(&self, message: &[u8]) -> Result<…, IdentityError>` — `identity.rs:190` | Ed25519 sign |
| `fn verify(&self, message: &[u8], signature: &[u8]) -> Result<bool, IdentityError>` — `identity.rs:202` | Ed25519 verify |

`Destination` — a local or remote destination. Defined at
`leviculum-core/src/destination.rs`. Re-exported as `leviculum_std::Destination`.

| Signature | Purpose |
|-----------|---------|
| `fn new(identity: Option<Identity>, direction: Direction, dest_type: DestinationType, app_name: &str, aspects: &[&str]) -> Result<Self, DestinationError>` — `destination.rs:291` | Construct a destination |
| `fn hash(&self) -> &DestinationHash` — `destination.rs:342` | Its 16-byte hash |
| `fn direction(&self) -> Direction` — `destination.rs:357` | In / Out |

`DestinationHash` — a 16-byte address (newtype, `destination.rs:158`):
`fn new(bytes: [u8; TRUNCATED_HASHBYTES]) -> Self` (`destination.rs:162`),
`fn as_bytes(&self) -> &[u8; TRUNCATED_HASHBYTES]` (`destination.rs:167`),
`fn into_bytes(self) -> [u8; TRUNCATED_HASHBYTES]` (`destination.rs:172`).
`Direction` (`destination.rs:160`) and `DestinationType` (`destination.rs:132`)
are the small enums passed to `Destination::new`. Packets are constructed
internally (`leviculum_core::packet::Packet`); applications work with
destinations and links, not raw packets.

### Platform traits

The three abstractions you implement to run the core on a platform. Defined in
`leviculum-core/src/traits.rs` and re-exported from `lib.rs:141`.

| Trait | Required methods (selected) | Source |
|-------|------------------------------|--------|
| `Clock` | `fn now_ms(&self) -> u64` | `traits.rs:162` |
| `Storage` | key-value persistence: `has_packet_hash`, `get_path`/`set_path`, link/announce tables, identities, ratchets (large trait) | `traits.rs:196` |
| `Interface` | `id`, `name`, `mtu`, `is_online`, `fn try_send(&mut self, data: &[u8]) -> Result<(), InterfaceError>` | `traits.rs:97` |

Provided `Storage` implementations: `NoStorage` (`traits.rs:783`, zero-sized
no-op for stubs and stateless devices), `MemoryStorage`
(`leviculum-core/src/memory_storage.rs`, BTreeMap-backed with caps), and
`EmbeddedStorage` (`leviculum-core/src/embedded_storage.rs:37`, `heapless`-backed
for flash-constrained targets; `fn new() -> Self` at `embedded_storage.rs:344`).
`leviculum-std` adds a file-backed `Storage` with Python-compatible on-disk
formats.

## Full rustdoc

This chapter covers the load-bearing surface; the exhaustive method list is the
generated rustdoc. Build and open it with:

```sh
cargo doc --no-deps --open -p leviculum-std    # std/tokio layer
cargo doc --no-deps --open -p leviculum-core   # no_std core
```
