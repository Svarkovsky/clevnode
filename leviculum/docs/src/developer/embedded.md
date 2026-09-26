# Embedded Development: Building on `leviculum-core`

This chapter is for building on `leviculum-core` directly: embedded firmware, a
custom async runtime, a simulator, or any host program that wants byte-level
control without tokio. The core is `no_std` (it uses `alloc`, but not the
standard library) and sans-IO — it performs no I/O and owns no runtime. You feed
it bytes, it hands back a [`TickOutput`](rust-api-spec.md#core-tickoutput-and-action),
and you do the I/O. The worked reference is the nRF52 firmware in `leviculum-nrf`,
cited throughout.

If you can use `std` and tokio, prefer `leviculum-std` and read the
[tutorial](rust-api-tutorial.md) instead — `leviculum-std` is itself a driver
for this same core. See [Choosing a layer](choosing-a-layer.md) for the trade-off.

## The dependency

Depend on `leviculum-core` with default features off. It is not on crates.io, so
use a path or git:

```toml
[dependencies]
# Adjust the path to wherever, and under whatever name, you cloned the repository
leviculum-core = { path = "../leviculum/leviculum-core", default-features = false }
```

No `std`, no tokio. You bring your own executor (Embassy, RTIC, a bare loop) and
your own allocator. The reference firmware `leviculum-nrf` targets
`thumbv7em-none-eabihf` and uses [Embassy](https://embassy.dev).

## The sans-IO contract

The core is a state machine with exactly three ways in, and one way out. The way
out is always a `TickOutput` (`leviculum-core/src/transport.rs:278`), carrying
`actions` to perform, `events` that occurred, and `next_deadline_ms`, the time at
which you must next tick the timer. It is `#[must_use]`: dropping it loses
outbound packets and events.

```text
received bytes ─► handle_packet(iface, data) ─┐
timer expired  ─► handle_timeout()            ├─► TickOutput { actions, events, next_deadline_ms }
                                              │
                                              └─► you: dispatch actions, react to events,
                                                       schedule the next timeout
```

The three entry points (signatures in the
[reference](rust-api-spec.md#nodecorer-c-s)):

- `handle_packet(iface, data)` — `leviculum-core/src/node/mod.rs:1032`. Feed one
  received frame, tagged with the [`InterfaceId`](rust-api-spec.md#core-tickoutput-and-action)
  it arrived on.
- `handle_timeout()` — `leviculum-core/src/node/mod.rs:1152`. Run periodic
  maintenance (path expiry, announce rebroadcasts, keepalives, retransmissions).
  Call it at or before `next_deadline`.
- `next_deadline()` — `leviculum-core/src/node/mod.rs:1183`. The earliest timer
  deadline in milliseconds, or `None` if no timer is pending. Sleep until this,
  or until a packet arrives, whichever comes first.

App-initiated operations (`register_destination`, `announce_destination`,
`connect`, `send_on_link`, `send_single_packet`) likewise return a `TickOutput`
you must dispatch.

## The driver loop

The shape is: compute the next deadline, wait for whichever of "a packet on any
interface" or "the deadline" happens first, call the matching entry point,
dispatch the resulting actions. This is exactly the `leviculum-nrf` T114 main
loop (`leviculum-nrf/src/bin/t114.rs:266-317`), here with three interfaces
(serial, LoRa, BLE) selected over with Embassy's `select4`:

```rust
// Adapted from leviculum-nrf/src/bin/t114.rs:266
loop {
    let deadline = node
        .next_deadline()
        .map(Instant::from_millis)
        .unwrap_or(Instant::MAX);

    match select4(
        serial.incoming_rx.receive(),
        lora_channels.incoming_rx.receive(),
        ble_channels.incoming_rx.receive(),
        Timer::at(deadline),
    )
    .await
    {
        Either4::First(data) => {
            let output = node.handle_packet(InterfaceId(0), &data);
            let mut ifaces: [&mut dyn Interface; 3] =
                [&mut serial_iface, &mut lora_iface, &mut ble_iface];
            dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
        }
        Either4::Second(data) => {
            let output = node.handle_packet(InterfaceId(1), &data);
            let mut ifaces: [&mut dyn Interface; 3] =
                [&mut serial_iface, &mut lora_iface, &mut ble_iface];
            dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
        }
        Either4::Third(data) => {
            let output = node.handle_packet(InterfaceId(2), &data);
            let mut ifaces: [&mut dyn Interface; 3] =
                [&mut serial_iface, &mut lora_iface, &mut ble_iface];
            dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
        }
        Either4::Fourth(()) => {
            let output = node.handle_timeout();
            let mut ifaces: [&mut dyn Interface; 3] =
                [&mut serial_iface, &mut lora_iface, &mut ble_iface];
            dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
        }
    }
}
```

Three things to notice:

1. **`next_deadline()` drives the timer.** Map `None` to "wait forever"
   (`Instant::MAX`) so you wake only when something actually needs doing — there
   is no fixed tick rate.
2. **`InterfaceId(n)` tags the source.** The index you pass to `handle_packet`
   must match the interface's own `id()`, so the core's routing tables and
   broadcast-exclusion stay consistent.
3. **`dispatch_actions` does the routing.** Rather than matching on each `Action`
   yourself, hand the whole `actions` vec plus your `&mut dyn Interface` slice to
   `dispatch_actions` (`leviculum-core/src/transport.rs:430`). Broadcast
   exclusion, interface selection, and IFAC wrapping live in core, so every
   driver gets them for free. Bind what it returns: the `DispatchResult` is
   `#[must_use]` because dropping it discards the retries the core asked for,
   the interface errors it saw, and any action it could not route.

This loop ignores `output.events` because a leaf firmware node has no application
logic to react to them; a richer firmware would drain `output.events` here the
way the [std event loop](rust-api-tutorial.md#step-2-take-the-event-receiver-and-consume-events)
drains the `EventReceiver`.

## Building the node

`NodeCoreBuilder` (`leviculum-core/src/node/builder.rs:40`) takes the platform
triple — RNG, [`Clock`](rust-api-spec.md#platform-traits), and
[`Storage`](rust-api-spec.md#platform-traits) — in its `build` call. From the
T114 firmware (`leviculum-nrf/src/bin/t114.rs:133-152`):

```rust
// Adapted from leviculum-nrf/src/bin/t114.rs:133
let mut builder = NodeCoreBuilder::new()
    .enable_transport(true)
    .max_incoming_resource_size(8 * 1024)
    .respond_to_probes(true);

if let Ok(Some(identity)) = id_store.load() {
    builder = builder.identity(identity);
}

let mut node = Box::new(builder.build(rng, EmbassyClock, EmbeddedStorage::new()));
```

`build` consumes the builder and the platform triple and returns the
`NodeCore<R, C, S>`.

## Implementing the platform traits

Three traits decouple the core from your hardware. Their signatures are in the
[reference](rust-api-spec.md#platform-traits); here is what to supply.

### `Clock`

A monotonic millisecond clock. The whole trait is one required method. The
nRF52 implementation wraps Embassy's timer
(`leviculum-nrf/src/clock.rs`):

```rust
use leviculum_core::traits::Clock;

pub struct EmbassyClock;

impl Clock for EmbassyClock {
    fn now_ms(&self) -> u64 {
        embassy_time::Instant::now().as_millis()
    }
}
```

`now_secs`, `has_elapsed`, and `deadline` have default implementations
(`leviculum-core/src/traits.rs:167-179`); you only provide `now_ms`. It must be
monotonic.

### `Interface`

The **send side** of an interface — `id`, `name`, `mtu`, `is_online`, and the
non-blocking `try_send` (`leviculum-core/src/traits.rs:277`). The receive side is
deliberately *not* in the trait: receiving is platform-specific (an interrupt, a
DMA buffer, an Embassy channel), and you feed received bytes into the core via
`handle_packet` yourself. `try_send` returns `InterfaceError::BufferFull`
(non-fatal, packet dropped — Reticulum is best-effort) or
`InterfaceError::Disconnected`. A minimal always-ready interface looks like the
test impl in `traits.rs`:

```rust
use leviculum_core::traits::{Interface, InterfaceError};
use leviculum_core::transport::InterfaceId;

struct MyRadio { /* hardware handle */ }

impl Interface for MyRadio {
    fn id(&self) -> InterfaceId { InterfaceId(1) }
    fn name(&self) -> &str { "my-radio" }
    fn mtu(&self) -> usize { 500 }
    fn is_online(&self) -> bool { true }
    fn try_send(&mut self, data: &[u8]) -> Result<(), InterfaceError> {
        // hand `data` to the radio's TX queue, non-blocking
        Ok(())
    }
}
```

A constrained medium (LoRa) overrides `next_slot_ms`
(`leviculum-core/src/traits.rs:335`) to report the next airtime-fit time, so the
core schedules retries against capacity without knowing any radio physics — the
[interface-isolation rule](choosing-a-layer.md). For a fast link the default
("always ready") is correct.

An interface that carries several point-to-point links behind one
`InterfaceId` (BLE) overrides `try_send_to_peer`
(`leviculum-core/src/traits.rs:316`): the core passes the 16-byte
identity of the peer it addressed the packet at — the same value the
interface reports on peer-up/peer-lost — or `None` for a broadcast.
The interface maps that identity to its own link(s); the core never
sees a link. The default implementation drops the hint, which is why
a single-peer interface implements nothing.

### `Storage`

Key-value persistence for the path table, link table, announce caches,
identities, ratchets, and dedup hashes (`leviculum-core/src/traits.rs:196`). It
is a large trait; you do not write it from scratch:

- `NoStorage` (`leviculum-core/src/traits.rs:783`) — zero-sized, every lookup
  returns nothing. Use it for a stateless node or a smoke test.
- `EmbeddedStorage` (`leviculum-core/src/embedded_storage.rs:37`,
  `EmbeddedStorage::new()` at `:344`) — `heapless`-backed, fixed-capacity, the
  production choice for flash-constrained devices. This is what the nRF52
  firmware uses.
- `MemoryStorage` (`leviculum-core/src/memory_storage.rs`) — BTreeMap-backed with
  configurable caps, for hosts with more memory.

Implement `Storage` yourself only to add real persistence (e.g. to flash); the
file-backed implementation in `leviculum-std` is the worked example of wrapping
`MemoryStorage` with disk writes.

## Summary

- Depend on `leviculum-core` with `default-features = false`. No `std`, no tokio,
  `alloc` required.
- Drive the loop: `next_deadline()` → wait for a packet or the deadline →
  `handle_packet` / `handle_timeout` → `dispatch_actions(output.actions)`.
- Implement `Clock` (trivial), `Interface` (send side only — you feed RX in via
  `handle_packet`), and pick a `Storage` (`NoStorage` / `EmbeddedStorage` /
  `MemoryStorage`, or your own).
- The full worked driver is `leviculum-nrf/src/bin/t114.rs`; the full method list
  is `cargo doc --no-deps -p leviculum-core` (see the
  [reference](rust-api-spec.md#full-rustdoc)).
