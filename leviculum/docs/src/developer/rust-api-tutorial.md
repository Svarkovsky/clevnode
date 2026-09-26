# Rust API Tutorial: Building on `leviculum-std`

This chapter builds a small application on `leviculum-std`, the std/tokio layer.
By the end you will have created a node, attached an interface, registered a
destination, sent both a single packet and link data, and consumed
[`NodeEvent`](rust-api-spec.md#eventreceiver-and-nodeevent)s. Every snippet is
adapted from a real example under `leviculum-std/examples/`; each step names the
file it comes from so you can read the full program. For exact signatures of
everything used here, see the [Rust API reference](rust-api-spec.md).

If you have not yet decided that `leviculum-std` is the right layer, read
[Choosing a layer](choosing-a-layer.md) first.

## Setup

Add the dependency and tokio. The crates are not on crates.io, so use a path
(workspace checkout) or git:

```toml
[dependencies]
# Adjust the path to wherever, and under whatever name, you cloned the repository
leviculum-std = { path = "../leviculum/leviculum-std" }
tokio = { version = "1", features = ["full"] }
tracing-subscriber = "0.3"
```

The examples all assume a running Reticulum daemon to attach to. Start a Python
`rnsd` (or a Leviculum `lnsd`) listening on `127.0.0.1:4242`, then run an example
with, for instance, `cargo run --example simple_send`.

## Step 1: build and start a node

The entry point is [`ReticulumNodeBuilder`](rust-api-spec.md#reticulumnodebuilder).
You add interfaces on the builder, call `build().await`, then `start().await`.
This is the opening of every example; here it is from `simple_send.rs`:

```rust
use leviculum_std::driver::ReticulumNodeBuilder;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    // Build a node with a TCP interface to a local daemon.
    let mut node = ReticulumNodeBuilder::new()
        .add_tcp_client("127.0.0.1:4242".parse()?)
        .build()
        .await?;

    node.start().await?;
    // ... use the node ...
    node.stop().await?;
    Ok(())
}
```

`build()` loads or generates the node's transport identity (persisted under the
storage path) and prepares interfaces, but does not run anything. `start()`
spawns the tokio event loop and brings the interfaces online. `stop()` flushes
state and tears the loop down. If you are constructing a node outside an async
context, `build_sync()` is the non-async equivalent of `build()`.

Other interfaces are added the same way:
`add_tcp_server(addr)`, `add_udp_interface(listen, forward)`,
`add_auto_interface()` (IPv6 multicast LAN discovery), and
`add_rnode_interface(...)` for LoRa. A relay node adds `enable_transport(true)`,
as in `relay_daemon.rs`:

```rust
// Adapted from relay_daemon.rs
let mut node = ReticulumNodeBuilder::new()
    .enable_transport(true)
    .add_tcp_client(peer)
    .build()
    .await?;
```

## Step 2: take the event receiver and consume events

Everything inbound — announces, paths, link lifecycle, link data — reaches you as
[`NodeEvent`](rust-api-spec.md#eventreceiver-and-nodeevent) values on an
[`EventReceiver`](rust-api-spec.md#eventreceiver-and-nodeevent). Take it once with
`take_event_receiver()` and call `recv().await` in a loop. From `simple_send.rs`:

```rust
let mut events = node
    .take_event_receiver()
    .ok_or("Failed to get event receiver")?;

while let Some(event) = events.recv().await {
    println!("Received event: {:?}", event);
}
```

`recv()` behaves like a `tokio::sync::mpsc::Receiver::recv` (it is cancel-safe in
`tokio::select!`) and returns `None` only once the node has shut down. The
`echo_server.rs` example shows the real shape: match on the variants you care
about and ignore the rest.

```rust
use leviculum_std::NodeEvent;

// Adapted from echo_server.rs
loop {
    tokio::select! {
        Some(event) = events.recv() => match event {
            NodeEvent::LinkEstablished { link_id, is_initiator } => {
                println!("link up: {:02x?} (we initiated: {})",
                    &link_id.as_bytes()[..4], is_initiator);
            }
            NodeEvent::LinkDataReceived { link_id, data } => {
                println!("{} bytes on {:02x?}: {:?}",
                    data.len(), &link_id.as_bytes()[..4],
                    String::from_utf8_lossy(&data));
            }
            NodeEvent::MessageReceived { link_id, msgtype, sequence, data } => {
                println!("msg type 0x{:04x} seq {} on {:02x?}",
                    msgtype, sequence, &link_id.as_bytes()[..4]);
            }
            NodeEvent::AnnounceReceived { announce, interface_index } => {
                println!("announce from {:02x?} on iface {}",
                    &announce.destination_hash().as_bytes()[..4], interface_index);
            }
            other => println!("other: {:?}", other),
        },
        _ = tokio::signal::ctrl_c() => break,
    }
}
```

Note the two receive variants. `MessageReceived` is the channel-multiplexed path
(sequenced, retransmitted) most link applications use; `LinkDataReceived` is the
lower-level raw-link-packet path (for example a Python peer calling
`RNS.Packet(link, data).send()`). The `chat.rs` example handles both.

## Step 3: register and announce a destination

To be reachable you register a local destination and announce it. A destination
is built from your identity, a direction, a type, an app name, and aspect
strings. This is from the `api` module's own test, which is the most compact
worked registration in the tree:

```rust
use leviculum_std::{Destination, Direction, DestinationType, generate_identity};

let id = generate_identity();

let dest = Destination::new(
    Some(id),
    Direction::In,
    DestinationType::Single,
    "leviculum-test",
    &["api"],
)?;
let dh = *dest.hash();              // 16-byte DestinationHash, read before moving dest

node.register_destination(dest);   // consumes dest

// Announce it; the optional payload rides along in the announce.
node.announce_destination(&dh, Some(b"hi")).await?;
```

Read `dest.hash()` before calling `register_destination`, which takes the
`Destination` by value. Incoming (`Direction::In`) destinations are auto-accepted
for links by the core (Python-RNS parity): when a peer opens a link to one, the
stack accepts and proves it automatically and you see a `LinkEstablished` event —
there is no separate accept call.

## Step 4: send a single packet

For fire-and-forget delivery use a [`PacketSender`](rust-api-spec.md#packetsender),
the single-packet handle. A path to the destination must already be known (learn
it from an announce, or call `request_path`). Adapted from the `PacketSender`
doctest in `driver/sender.rs`:

```rust
let endpoint = node.packet_sender(&dest_hash);
let _packet_hash = endpoint.send(b"Hello!").await?;
```

`send` returns the truncated packet hash, which you can match against a later
`PacketDeliveryConfirmed` event if the destination proves delivery.

## Step 5: open a link and send on it

A link is an encrypted session. Open one with `connect`, passing the destination
hash and its 32-byte Ed25519 signing key (the signing half of the peer's
identity, learned from its announce). You get back a
[`LinkHandle`](rust-api-spec.md#linkhandle). Adapted from the `LinkHandle`
doctest in `driver/stream.rs`:

```rust
let handle = node.connect(&dest_hash, &signing_key).await?;

// The handle is usable immediately, but the link is not yet established.
// Watch for NodeEvent::LinkEstablished on the event receiver before relying
// on delivery, then send:
handle.send(b"Hello!").await?;
```

`connect` returns as soon as the link request is dispatched; the link is *pending*
until a `LinkEstablished` event fires for its `link_id`. `send` absorbs pacing and
busy conditions by retrying internally; `try_send` is the non-blocking variant
that surfaces backpressure instead. Responses arrive as `MessageReceived` /
`LinkDataReceived` events on the receiver you took in step 2. Close with
`handle.close().await` when done.

On the responder side you do not call `connect`. Once a `LinkEstablished` event
fires for a link you did not initiate (`is_initiator == false`), the link is
already live; mint a writable handle for it with `node.link_handle(&link_id)` and
send on that.

## Where to go next

- `simple_send.rs` and `echo_server.rs` — the minimal node + event loop.
- `chat.rs` — both receive variants, node status (`active_link_count`,
  `pending_link_count`).
- `relay_daemon.rs` — a transport node and `transport_stats()`.
- `link_test.rs` / `link_integration_test.rs` — these drop down to
  `leviculum-core`'s `Link` directly against a Python `rnsd`, useful if you want
  to see the wire-level handshake rather than the high-level handle API.

The full method list of every type is in the generated rustdoc. Build it with:

```sh
cargo doc --no-deps --open -p leviculum-std
```

For verified signatures of the types used above, continue to the
[Rust API reference](rust-api-spec.md).
