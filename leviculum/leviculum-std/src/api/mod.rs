//! Curated consumer facade for Leviculum.
//!
//! This module is the stable, minimal public API meant for application
//! developers and for the C FFI in `leviculum-ffi`. It selects the small set
//! of app relevant entry points out of the larger `driver` surface and gives
//! them types that hide core internals, so consumers never depend on
//! `leviculum_core` directly.
//!
//! It is additive and adds no behaviour: every method is a thin re-projection
//! of the existing engine in [`crate::driver`]. The design of record is
//! `docs/leviculum-api-design.md`.
//!
//! v1 scope grows in phases: this module currently covers instance lifecycle,
//! identity, version, destinations and announce, paths, links, datagrams, and
//! requests and responses. Resource transfer and packaging land in later
//! phases. The event stream is consumed by the FFI via the engine receiver.

use std::path::PathBuf;

use crate::driver::{ReticulumNode, ReticulumNodeBuilder};

pub use crate::config::InterfaceConfig;
pub use crate::driver::{CoreProcessor, StdNodeCore};
pub use crate::error::{Error as ApiError, Result};
pub use crate::{Destination, DestinationHash, DestinationType, Direction, LinkHandle, LinkId};
pub use leviculum_core::node::NodeEvent;
pub use leviculum_core::resource::ResourceStrategy;
pub use leviculum_core::transport::InterfaceId;
pub use leviculum_core::transport::TickOutput;
pub use leviculum_core::{Identity, RequestPolicy};

/// Generate a new random identity using the system RNG.
///
/// Convenience re-export of [`crate::generate_identity`] under the facade.
pub fn generate_identity() -> Identity {
    crate::generate_identity()
}

/// Semantic version of the facade, as `(major, minor, patch)`.
///
/// Sourced from the crate version at compile time.
pub fn version() -> (u16, u16, u16) {
    (
        env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0),
        env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or(0),
        env!("CARGO_PKG_VERSION_PATCH").parse().unwrap_or(0),
    )
}

/// Version string of the facade, for example `"0.7.0"`.
pub fn version_string() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Builder for a [`Node`].
///
/// Thin wrapper over [`ReticulumNodeBuilder`] that exposes only the
/// app relevant configuration. Consumes itself on each setter, builder style.
#[derive(Default)]
pub struct NodeBuilder {
    inner: ReticulumNodeBuilder,
}

impl NodeBuilder {
    /// Create a new builder with defaults.
    pub fn new() -> Self {
        Self {
            inner: ReticulumNodeBuilder::new(),
        }
    }

    /// Use an explicit identity instead of generating one.
    pub fn identity(mut self, identity: Identity) -> Self {
        self.inner = self.inner.identity(identity);
        self
    }

    /// Set the storage directory for identity, known destinations, and ratchets.
    pub fn storage_path(mut self, path: PathBuf) -> Self {
        self.inner = self.inner.storage_path(path);
        self
    }

    /// Add a TCP client interface to a remote Reticulum node.
    pub fn add_tcp_client(mut self, addr: std::net::SocketAddr) -> Self {
        self.inner = self.inner.add_tcp_client(addr);
        self
    }

    /// Add a TCP server interface listening for inbound connections.
    pub fn add_tcp_server(mut self, addr: std::net::SocketAddr) -> Self {
        self.inner = self.inner.add_tcp_server(addr);
        self
    }

    /// Add a UDP interface (one datagram is one packet).
    pub fn add_udp(
        mut self,
        listen_addr: std::net::SocketAddr,
        forward_addr: std::net::SocketAddr,
    ) -> Self {
        self.inner = self.inner.add_udp_interface(listen_addr, forward_addr);
        self
    }

    /// Add an AutoInterface (IPv6 multicast LAN discovery) with defaults.
    pub fn add_auto_interface(mut self) -> Self {
        self.inner = self.inner.add_auto_interface();
        self
    }

    /// Add an RNode (LoRa) interface programmatically, so a C app reaches LoRa
    /// without a config file. `port` is the serial device; the rest are the
    /// required radio settings. Optional tuning stays at driver defaults.
    pub fn add_rnode(
        mut self,
        port: &str,
        frequency: u64,
        bandwidth: u32,
        spreading_factor: u8,
        coding_rate: u8,
        tx_power: i8,
    ) -> Self {
        self.inner = self.inner.add_rnode_interface(
            port.to_string(),
            frequency,
            bandwidth,
            spreading_factor,
            coding_rate,
            tx_power,
        );
        self
    }

    /// Add a serial interface (KISS framing over a raw serial port).
    pub fn add_serial(
        mut self,
        port: &str,
        speed: u32,
        databits: u8,
        parity: &str,
        stopbits: u8,
    ) -> Self {
        self.inner = self.inner.add_serial_interface(
            port.to_string(),
            speed,
            databits,
            parity.to_string(),
            stopbits,
        );
        self
    }

    /// Enable or disable transport (relay and routing) mode.
    pub fn enable_transport(mut self, enabled: bool) -> Self {
        self.inner = self.inner.enable_transport(enabled);
        self
    }

    /// Register an outbound-socket hook. See
    /// [`ReticulumNodeBuilder::outbound_socket_hook`](crate::driver::ReticulumNodeBuilder::outbound_socket_hook).
    pub fn outbound_socket_hook(mut self, hook: crate::socket_hook::OutboundSocketHook) -> Self {
        self.inner = self.inner.outbound_socket_hook(hook);
        self
    }

    /// Install an in-driver core processor. See [`CoreProcessor`] for what a
    /// hook body may and may not do.
    ///
    /// The event stream is not an alternative feed for one: it classifies
    /// `PacketReceived` and `LinkDataReceived` as `EventClass::Data`, which is
    /// droppable under load and, for a messaging stack, lost messages.
    ///
    /// [`StdNodeCore`], [`NodeEvent`] and [`TickOutput`] are re-exported for
    /// the `impl`. `TickOutput` appears nowhere else in this crate's public
    /// surface, so without it writing one still means depending on
    /// `leviculum-core` directly.
    pub fn core_processor(mut self, processor: impl CoreProcessor) -> Self {
        self.inner = self.inner.core_processor(processor);
        self
    }

    /// Override the link keepalive interval (seconds) for every link. Shrinks
    /// the stale-link timeout proportionally; useful for slow links and for
    /// making stale/recovery observable quickly.
    pub fn link_keepalive(mut self, secs: u64) -> Self {
        self.inner = self.inner.link_keepalive(secs);
        self
    }

    /// Load interface and node configuration from an INI config file, the same
    /// format `rnsd`/`lnsd` use. This brings every interface type, including
    /// RNode and serial, into the node, so a C app can adopt the user's
    /// existing Reticulum configuration.
    pub fn config_file(mut self, path: PathBuf) -> Self {
        self.inner = self.inner.config_file(path);
        self
    }

    /// Run as a shared instance under `name`: expose the local IPC socket and
    /// the RPC server (so `rnstatus`/`rnpath`/`rnprobe` and other tools can use
    /// this node's transport), in addition to the node's own interfaces.
    pub fn share_instance(mut self, name: &str) -> Self {
        self.inner = self
            .inner
            .share_instance(true)
            .instance_name(name.to_string());
        self
    }

    /// Connect to a running shared instance `name` instead of bringing up own
    /// interfaces, routing everything through that daemon. This is how a
    /// drop-in tool reuses a host's existing Reticulum stack.
    pub fn connect_to_shared_instance(mut self, name: &str) -> Self {
        self.inner = self.inner.connect_to_shared_instance(name);
        self
    }

    /// Build the node without entering an async context.
    ///
    /// The node is created and its identity loaded or generated, but the event
    /// loop is not started. Call [`Node::start`] to bring it online.
    pub fn build(self) -> Result<Node> {
        Ok(Node {
            inner: self.inner.build_sync()?,
        })
    }
}

/// A running or stopped Reticulum node.
///
/// Thin wrapper over [`ReticulumNode`] exposing the app relevant lifecycle.
/// The tokio runtime and event loop are owned internally.
pub struct Node {
    inner: ReticulumNode,
}

impl Node {
    /// Start the node: spawn the event loop and bring up interfaces.
    pub async fn start(&mut self) -> Result<()> {
        self.inner.start().await
    }

    /// Stop the node, persist state, and tear down the event loop.
    pub async fn stop(&mut self) -> Result<()> {
        self.inner.stop().await
    }

    /// Whether the event loop is running.
    pub fn is_running(&self) -> bool {
        self.inner.is_running()
    }

    /// The node's own identity hash (16 bytes).
    pub fn identity_hash(&self) -> [u8; 16] {
        self.inner.identity_hash()
    }

    /// Take the engine event receiver, once.
    ///
    /// The C FFI bridge owns this to drain events onto its pollable fd. Returns
    /// `None` if already taken or if the node was built without events.
    pub fn take_event_receiver(&mut self) -> Option<crate::driver::EventReceiver> {
        self.inner.take_event_receiver()
    }

    /// Attach a TCP client interface to the running node, optionally egressing
    /// through a SOCKS5 proxy. See
    /// [`ReticulumNode::spawn_tcp_client`](crate::driver::ReticulumNode::spawn_tcp_client).
    pub fn spawn_tcp_client(
        &self,
        name: &str,
        host: &str,
        port: u16,
        socks_proxy: Option<(String, u16)>,
    ) -> Result<crate::interfaces::TcpClientHandle> {
        self.inner.spawn_tcp_client(name, host, port, socks_proxy)
    }

    /// Attach a PipeInterface subprocess to the running node. See
    /// [`ReticulumNode::spawn_pipe_client`](crate::driver::ReticulumNode::spawn_pipe_client).
    pub fn spawn_pipe_client(
        &self,
        name: &str,
        command: &str,
        respawn_delay: Option<std::time::Duration>,
    ) -> Result<crate::interfaces::PipeClientHandle> {
        self.inner.spawn_pipe_client(name, command, respawn_delay)
    }

    /// Attach any configured interface type to the running node. See
    /// [`ReticulumNode::spawn_interface`](crate::driver::ReticulumNode::spawn_interface).
    #[must_use = "the returned ids are the only handle for removing the interface"]
    pub fn spawn_interface(&self, config: InterfaceConfig) -> Result<Vec<InterfaceId>> {
        self.inner.spawn_interface(config)
    }

    /// Detach an interface by id. See
    /// [`ReticulumNode::remove_interface`](crate::driver::ReticulumNode::remove_interface).
    pub fn remove_interface(&self, id: InterfaceId) -> Result<()> {
        self.inner.remove_interface(id)
    }

    /// Register a local destination so the node can announce it and accept
    /// links or packets for it.
    ///
    /// Incoming destinations are set to accept links (auto-accept model,
    /// Python-RNS parity): the stack accepts and proves an inbound link, then
    /// surfaces a `LinkEstablished` event the app uses (there is no separate
    /// accept decision). Without registration the engine silently drops
    /// incoming link requests.
    pub fn register_destination(&self, destination: Destination) {
        self.register_destination_with_links(destination, true);
    }

    /// Register a local destination, choosing whether inbound links are accepted.
    ///
    /// Like [`register_destination`](Self::register_destination) but with an
    /// explicit accept switch for incoming (`Direction::In`) destinations.
    /// `accepts_links == true` is the auto-accept default (the stack accepts and
    /// proves inbound links); `false` keeps the destination reachable for
    /// announces and packets while declining inbound links (Codeberg #21, "keep
    /// the option to disable accepting"). The flag is ignored for outbound
    /// destinations, which never accept links.
    pub fn register_destination_with_links(&self, destination: Destination, accepts_links: bool) {
        let mut destination = destination;
        if destination.direction() == Direction::In {
            destination.set_accepts_links(accepts_links);
        }
        self.inner.register_destination(destination);
    }

    /// Announce a registered destination on all interfaces.
    ///
    /// `app_data` is optional application payload carried in the announce.
    pub async fn announce(
        &self,
        dest_hash: &DestinationHash,
        app_data: Option<&[u8]>,
    ) -> Result<()> {
        self.inner.announce_destination(dest_hash, app_data).await
    }

    /// Whether a path to the destination is known.
    pub fn has_path(&self, dest_hash: &DestinationHash) -> bool {
        self.inner.has_path(dest_hash)
    }

    /// Hop count to the destination, if a path is known.
    pub fn hops_to(&self, dest_hash: &DestinationHash) -> Option<u8> {
        self.inner.hops_to(dest_hash)
    }

    /// The current ratchet public key of a registered local destination, if
    /// ratchets are enabled on it.
    pub fn destination_ratchet_public(&self, dest_hash: &DestinationHash) -> Option<[u8; 32]> {
        self.inner.destination_ratchet_public(dest_hash)
    }

    /// The KNOWN REMOTE ratchet public key for a destination, learned from a
    /// ratcheted announce. This is what the send path uses to encrypt to a
    /// remote peer; unlike `destination_ratchet_public` it is not restricted to
    /// local destinations.
    pub fn known_remote_ratchet(&self, dest_hash: &DestinationHash) -> Option<[u8; 32]> {
        self.inner.known_remote_ratchet(dest_hash)
    }

    /// A read-only snapshot of the transport counters (packets sent, received,
    /// forwarded, announces processed, packets dropped).
    pub fn transport_stats(&self) -> leviculum_core::transport::TransportStats {
        self.inner.transport_stats()
    }

    /// The number of known paths in the path table.
    pub fn path_count(&self) -> usize {
        self.inner.path_count()
    }

    /// A point-in-time snapshot of the path table (one entry per known
    /// destination), for an `rnpath`-style view.
    pub fn path_table(&self) -> Vec<leviculum_core::transport::PathTableExport> {
        self.inner.path_table_entries()
    }

    /// A read-only snapshot of every interface (name, online status, byte
    /// counters), for an `rnstatus`-style interface view.
    pub fn interface_stats(&self) -> Vec<crate::driver::InterfaceStatusSnapshot> {
        self.inner.interface_stats()
    }

    /// The bound addresses of this node's TCP server listeners, in start
    /// order. A server added with port 0 reports the kernel-assigned port
    /// here after `start()`. See
    /// [`crate::driver::ReticulumNode::tcp_listen_addrs`].
    pub fn tcp_listen_addrs(&self) -> Vec<std::net::SocketAddr> {
        self.inner.tcp_listen_addrs()
    }

    /// Change the announce bandwidth cap on a registered interface at runtime.
    /// See [`crate::driver::ReticulumNode::set_interface_announce_cap`].
    pub fn set_interface_announce_cap(
        &self,
        iface_id: leviculum_core::transport::InterfaceId,
        cap_percent: u32,
    ) -> bool {
        self.inner.set_interface_announce_cap(iface_id, cap_percent)
    }

    /// The cached identity for a destination, learned from an announce.
    pub fn get_identity(&self, dest_hash: &DestinationHash) -> Option<Identity> {
        self.inner.get_identity(dest_hash)
    }

    /// Attach a byte-channel interface over a caller-supplied duplex stream. See
    /// [`ReticulumNode::spawn_byte_channel`](crate::driver::ReticulumNode::spawn_byte_channel).
    #[must_use = "dropping the handle immediately detaches the interface"]
    pub fn spawn_byte_channel<S>(
        &self,
        name: &str,
        stream: S,
    ) -> Result<crate::interfaces::ByteChannelHandle>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        self.inner.spawn_byte_channel(name, stream)
    }

    /// Request a path to a destination. The result arrives as a path-found event.
    pub async fn request_path(&self, dest_hash: &DestinationHash) -> Result<()> {
        self.inner.request_path(dest_hash).await
    }

    /// Wait until a path to `dest_hash` is known, actively re-issuing a
    /// PATH_REQUEST on a bounded cadence if it does not arrive passively.
    ///
    /// Returns `Ok(true)` once a path is known, or `Ok(false)` if `timeout`
    /// elapses first. The path arriving passively within the first
    /// `retry_interval` never triggers a PATH_REQUEST, so the common case is
    /// unchanged; a delayed announce (e.g. an upstream Python `rnsd` holding a
    /// forwarded announce under ingress limiting, Codeberg #44) is recovered by
    /// the explicit request, which is answered over the path-response path that
    /// bypasses the hold. Client-side only, with no medium awareness.
    pub async fn wait_for_path(
        &self,
        dest_hash: &DestinationHash,
        timeout: std::time::Duration,
        retry_interval: std::time::Duration,
    ) -> Result<bool> {
        self.inner
            .wait_for_path(dest_hash, timeout, retry_interval)
            .await
    }

    /// Open a link to a destination, given its Ed25519 signing key.
    pub async fn connect_with_key(
        &self,
        dest_hash: &DestinationHash,
        signing_key: &[u8; 32],
    ) -> Result<LinkHandle> {
        self.inner.connect(dest_hash, signing_key).await
    }

    /// Obtain a writable handle to an already-established inbound link.
    ///
    /// Inbound links are auto-accepted and auto-proved by the stack (Python-RNS
    /// parity): once a `LinkEstablished` event fires for a link this node did
    /// not initiate, the link is live. Call this to mint a [`LinkHandle`] for
    /// that link so the application can send on it. Purely a handle
    /// constructor; it has no wire side effect (the establishment proof was
    /// already sent by the core).
    pub fn accept_link(&self, link_id: &LinkId) -> LinkHandle {
        self.inner.link_handle(link_id)
    }

    /// Prove an identity to the peer on a link. The peer sees a link-identified
    /// event and can read the identity via [`Node::get_remote_identity`].
    pub async fn identify_link(&self, link_id: &LinkId, identity: &Identity) -> Result<()> {
        self.inner.identify_link(link_id, identity).await
    }

    /// The peer's identity on a link, if they have identified.
    pub fn get_remote_identity(&self, link_id: &LinkId) -> Option<Identity> {
        self.inner.get_remote_identity(link_id)
    }

    /// Send one unreliable datagram to a destination, returning the packet hash.
    /// A path to the destination must already be known.
    pub async fn send_datagram(
        &self,
        dest_hash: &DestinationHash,
        data: &[u8],
    ) -> Result<[u8; 16]> {
        self.inner.send_single_packet(dest_hash, data).await
    }

    /// Send a delivery proof for a received packet, after a
    /// `PacketProofRequested` event on a destination with the App proof
    /// strategy.
    pub async fn send_proof(
        &self,
        dest_hash: &DestinationHash,
        packet_hash: &[u8; 32],
    ) -> Result<()> {
        // The engine's send_proof takes (packet_hash, dest_hash); this facade
        // orders its parameters (dest_hash, packet_hash) to match the C ABI.
        // The call below maps each named argument to its engine slot, so the
        // proof is addressed to `dest_hash`. The two have distinct types, so a
        // genuine swap would not type-check.
        self.inner.send_proof(packet_hash, dest_hash).await
    }

    /// Register a handler for requests to `path` on a local destination.
    pub fn register_request_handler(
        &self,
        dest_hash: DestinationHash,
        path: &str,
        policy: RequestPolicy,
    ) {
        self.inner.register_request_handler(dest_hash, path, policy);
    }

    /// Remove the handler for `path`, returning whether one was registered.
    pub fn deregister_request_handler(&self, dest_hash: DestinationHash, path: &str) -> bool {
        self.inner.deregister_request_handler(dest_hash, path)
    }

    /// Send a request on an established link, returning the request id. The
    /// response or a timeout arrives as an event.
    pub async fn send_request(
        &self,
        link_id: &LinkId,
        path: &str,
        data: Option<&[u8]>,
        timeout_ms: Option<u64>,
    ) -> Result<[u8; 16]> {
        self.inner
            .send_request(link_id, path, data, timeout_ms)
            .await
    }

    /// Send a response to a received request. `response_data` must be one valid
    /// msgpack-encoded value.
    pub async fn send_response(
        &self,
        link_id: &LinkId,
        request_id: &[u8; 16],
        response_data: &[u8],
    ) -> Result<()> {
        self.inner
            .send_response(link_id, request_id, response_data)
            .await
    }

    /// Answer a received request with a response Resource, for responses
    /// larger than the link MDU. Use after `send_response` returns
    /// `PayloadTooLarge`; the `[request_id, response]` msgpack wrapper is
    /// added internally. `response_data` must be one valid msgpack-encoded
    /// value.
    pub async fn send_response_resource(
        &self,
        link_id: &LinkId,
        request_id: &[u8; 16],
        response_data: &[u8],
    ) -> Result<()> {
        self.inner
            .send_response_resource(link_id, request_id, response_data)
            .await
    }

    /// Send a file-style response to a received request: a response Resource of
    /// the RAW bytes plus msgpack-encoded `metadata` (no `[request_id,
    /// response]` wrapper), the wire form NomadNet's `serve_file` uses.
    pub async fn send_file_response(
        &self,
        link_id: &LinkId,
        request_id: &[u8; 16],
        data: &[u8],
        metadata: &[u8],
    ) -> Result<()> {
        self.inner
            .send_file_response(link_id, request_id, data, metadata)
            .await
    }

    /// Send a resource over an established link, returning the resource hash.
    /// `metadata`, if present, must be msgpack-encoded by the caller.
    pub async fn send_resource(
        &self,
        link_id: &LinkId,
        data: &[u8],
        metadata: Option<&[u8]>,
        auto_compress: bool,
    ) -> Result<[u8; 32]> {
        self.inner
            .send_resource(link_id, data, metadata, auto_compress)
            .await
    }

    /// Set the acceptance strategy for incoming resources on a link.
    pub fn set_resource_strategy(
        &self,
        link_id: &LinkId,
        strategy: ResourceStrategy,
    ) -> Result<()> {
        self.inner.set_resource_strategy(link_id, strategy)
    }

    /// Accept a pending resource advertised on a link (for the AcceptApp strategy).
    pub async fn accept_resource(&self, link_id: &LinkId) -> Result<()> {
        self.inner.accept_resource(link_id).await
    }

    /// Reject a pending resource advertised on a link.
    pub async fn reject_resource(&self, link_id: &LinkId) -> Result<()> {
        self.inner.reject_resource(link_id).await
    }

    /// Access the underlying engine node.
    ///
    /// Escape hatch while the facade is incomplete: later phases re-project the
    /// remaining methods (destinations, links, events) so consumers will not
    /// need this. Not part of the stable surface.
    #[doc(hidden)]
    pub fn engine(&self) -> &ReticulumNode {
        &self.inner
    }

    /// Mutable access to the underlying engine node. See [`Node::engine`].
    #[doc(hidden)]
    pub fn engine_mut(&mut self) -> &mut ReticulumNode {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matches_crate() {
        let (major, minor, patch) = version();
        let s = format!("{major}.{minor}.{patch}");
        assert_eq!(s, version_string());
    }

    #[test]
    fn generated_identity_has_private_keys() {
        let id = generate_identity();
        assert!(id.has_private_keys());
        assert_eq!(id.hash().len(), 16);
    }

    #[tokio::test]
    async fn register_and_announce_single_destination() {
        let id = generate_identity();
        let mut node = NodeBuilder::new()
            .identity(id.clone())
            .storage_path(std::env::temp_dir().join("leviculum-api-test-announce"))
            .enable_transport(false)
            .build()
            .expect("build node");
        node.start().await.expect("start node");

        let dest = Destination::new(
            Some(id),
            Direction::In,
            DestinationType::Single,
            "leviculum-test",
            &["api"],
        )
        .expect("build destination");
        let dh = *dest.hash();
        node.register_destination(dest);
        // With no interfaces the announce reaches nobody, but the action path
        // must succeed.
        node.announce(&dh, Some(b"hi")).await.expect("announce");

        node.stop().await.expect("stop node");
    }

    #[tokio::test]
    async fn node_lifecycle_without_interfaces() {
        let mut node = NodeBuilder::new()
            .storage_path(std::env::temp_dir().join("leviculum-api-test-lifecycle"))
            .enable_transport(false)
            .build()
            .expect("build node");
        assert!(!node.is_running());
        node.start().await.expect("start node");
        assert!(node.is_running());
        // Identity hash is stable and 16 bytes.
        assert_eq!(node.identity_hash().len(), 16);
        node.stop().await.expect("stop node");
        assert!(!node.is_running());

        // Restart: stop then start brings the node back up (the engine
        // rebuilds its runtime on start).
        node.start().await.expect("restart node");
        assert!(node.is_running());
        node.stop().await.expect("stop node again");
        assert!(!node.is_running());
    }
}
