# Building on Leviculum in Rust: Choosing a Layer

Leviculum is a Rust workspace, not a single crate. The Reticulum stack is split
into layers so that the same protocol engine can run on a tokio server, a
bare-metal nRF52 radio, or behind a C ABI. As an application developer your
first decision is which layer you build against. This chapter explains the four
crates, the dependency direction between them, and gives a decision table.

The companion chapters are the [Rust API tutorial](rust-api-tutorial.md) (a
hands-on `leviculum-std` walkthrough), the [Rust API reference](rust-api-spec.md)
(verified signatures of the key types), and [Embedded development](embedded.md)
(building on `leviculum-core` directly). If you are writing C rather than Rust,
the [C API overview](../c-api/overview.md) and [How-To](../c-api/howto.md) are
your counterparts to those chapters.

## The four layers

```
        leviculum-ffi  (C ABI)        leviculum-nrf  (nRF52 firmware)
              │                              │
              ▼                              │
        leviculum-std  (std, tokio)         │
              │                              │
              ▼                              ▼
                    leviculum-core  (no_std, sans-IO)
```

The dependency direction is strict and one-way. `leviculum-std` builds on
`leviculum-core`; `leviculum-ffi` wraps `leviculum-std`; `leviculum-nrf` wraps
`leviculum-core` directly (it never pulls in `std` or tokio). Nothing depends on
a layer above it.

### `leviculum-core` — the no_std, sans-IO engine

`leviculum-core` is the protocol. It is `no_std` (it pulls in `alloc`, but not
the standard library), performs no I/O of its own, and owns no runtime. It is
sans-IO: you feed it received bytes, it returns a
[`TickOutput`](rust-api-spec.md#core-tickoutput-and-action) describing the
packets to send and the events that occurred, and you dispatch those yourself.
Time, persistence, and the network are abstracted behind three traits —
[`Clock`](rust-api-spec.md#core-traits), [`Storage`](rust-api-spec.md#core-traits),
and [`Interface`](rust-api-spec.md#core-traits) — that you implement for your
platform.

Build against `leviculum-core` when you have your own runtime or event loop and
do not want tokio: embedded firmware, an integration into a different async
executor, a simulator, or a host program that wants byte-level control. See
[Embedded development](embedded.md).

### `leviculum-std` — the full std/tokio application layer

`leviculum-std` is what most Rust applications use. It supplies the platform
pieces `leviculum-core` abstracts: a `SystemClock`, file-backed storage with
Python-compatible on-disk formats, and concrete interfaces (TCP client and
server, UDP, AutoInterface for LAN discovery, RNode/LoRa, raw serial). On top of
those it runs the sans-IO core inside a tokio event loop and exposes an async,
handle-based API: build a node with [`ReticulumNodeBuilder`](rust-api-spec.md#reticulumnodebuilder),
`start()` it, take an [`EventReceiver`](rust-api-spec.md#eventreceiver-and-nodeevent),
and use [`LinkHandle`](rust-api-spec.md#linkhandle) / [`PacketSender`](rust-api-spec.md#packetsender)
to send.

Build against `leviculum-std` when you are writing a normal Rust program on
Linux/macOS that talks to a Reticulum mesh. This is the path the
[tutorial](rust-api-tutorial.md) and the examples under
`leviculum-std/examples/` take.

### `leviculum-ffi` — the C ABI wrapper

`leviculum-ffi` exposes `leviculum-std` through a C-compatible ABI: opaque
handles, integer error codes, a pollable event fd. It is the layer behind
`leviculum.h` and `libleviculum.so`. If you are writing Rust you do not use it —
you use `leviculum-std` directly, which is what `leviculum-ffi` itself does
internally. It exists so that non-Rust programs (C, and anything that can call a
C library) get the same engine.

If your application is in C, stop here and read the
[C API overview](../c-api/overview.md) and [How-To](../c-api/howto.md) instead;
they are the C counterpart to this Rust documentation.

### `leviculum-nrf` — the reference firmware

`leviculum-nrf` is standalone firmware for nRF52 boards (the T114 and RAK4631
LoRa nodes), built with the [Embassy](https://embassy.dev) async embedded
framework. It targets `thumbv7em-none-eabihf` and depends on
`leviculum-core` directly with `default-features = false` — no `std`, no tokio.
It is both a usable firmware and the worked reference for how to drive the
sans-IO core on bare metal; the [embedded chapter](embedded.md) walks through its
main loop.

You do not "build on" `leviculum-nrf` the way you build on a library; you fork it
or read it as the canonical example of a `leviculum-core` integration on a real
device.

## Decision table

| You are building… | Use | Why |
|-------------------|-----|-----|
| A Linux/macOS app or daemon talking to a mesh | `leviculum-std` | Async handle API, real interfaces, file storage, tokio loop already wired |
| A drop-in tool reusing a running `lnsd`/`rnsd` | `leviculum-std` | `connect_to_shared_instance` over the shared-instance IPC |
| A relay / transport node | `leviculum-std` | `enable_transport(true)`, see `relay_daemon.rs` |
| A C program (any non-Rust language with C FFI) | `leviculum-ffi` | Stable C ABI, opaque handles, pollable fd — see the [C API chapters](../c-api/overview.md) |
| Firmware on an nRF52 LoRa board | `leviculum-nrf` | Reference firmware; fork or adapt it |
| Firmware on a different MCU / a custom async runtime | `leviculum-core` | Implement `Clock`/`Storage`/`Interface`, drive the sans-IO loop yourself |
| A simulator or byte-level test harness with no I/O | `leviculum-core` | Feed bytes, inspect `TickOutput`, no runtime imposed |

## Adding the dependency

None of these crates are published on crates.io. Depend on them by path (in a
workspace checkout) or by git. For a `leviculum-std` application:

```toml
# By path — adjust to wherever, and under whatever name, you cloned the
# repository; this example assumes a sibling directory named `leviculum`
[dependencies]
leviculum-std = { path = "../leviculum/leviculum-std" }
tokio = { version = "1", features = ["full"] }

# Or by git
# leviculum-std = { git = "https://codeberg.org/Lew_Palm/leviculum" }
```

For embedded work depend on `leviculum-core` instead, with default features off:

```toml
[dependencies]
leviculum-core = { path = "../leviculum/leviculum-core", default-features = false }
```

The workspace is edition 2021 and licensed AGPL-3.0-or-later. Version numbers
are the crate manifests' to state, not this page's — read them from
`Cargo.toml` (`leviculum-nrf` versions independently of the workspace).
