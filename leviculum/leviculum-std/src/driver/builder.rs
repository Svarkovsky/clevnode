//! Builder for ReticulumNode
//!
//! Provides fluent configuration for creating ReticulumNode instances.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use leviculum_core::identity::Identity;
use leviculum_core::node::NodeCoreBuilder;
use leviculum_core::resource::WindowPolicy;
use leviculum_core::ProofStrategy;

use crate::clock::SystemClock;
use crate::config::{Config, InterfaceConfig};
use crate::error::Error;
use crate::interfaces::rnode::{
    RNodeChannelConfig, RNodeChannelFactory, RNODE_DEFAULT_BUFFER_SIZE,
};
use crate::storage::Storage;

use super::{CoreProcessor, ReticulumNode};

/// Builder for creating ReticulumNode instances
///
/// # Example
///
/// ```no_run
/// use leviculum_std::driver::ReticulumNodeBuilder;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let node = ReticulumNodeBuilder::new()
///     .add_tcp_client("127.0.0.1:4242".parse().unwrap())
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
pub struct ReticulumNodeBuilder {
    core_builder: NodeCoreBuilder,
    /// Pre-loaded config (takes priority over config_path)
    loaded_config: Option<Config>,
    config_path: Option<PathBuf>,
    storage_path: Option<PathBuf>,
    interfaces: Vec<InterfaceConfig>,
    /// Channel-backed RNode interfaces (host-supplied byte channels). Kept
    /// separate from `interfaces` because a factory closure is not
    /// representable in the serializable `InterfaceConfig`.
    rnode_channels: Vec<RNodeChannelConfig>,
    corrupt_every: Option<u64>,
    outbound_socket_hook: Option<crate::socket_hook::OutboundSocketHook>,
    /// Explicit enable_transport override (takes priority over config value)
    enable_transport_explicit: Option<bool>,
    /// Explicit shared_instance override (takes priority over config value)
    share_instance_explicit: Option<bool>,
    /// Explicit instance_name override (takes priority over config value)
    instance_name_explicit: Option<String>,
    /// Explicit flush_interval_secs override (takes priority over config value)
    flush_interval_secs_explicit: Option<u64>,
    /// Explicit control-channel capacity override (takes priority over config)
    control_channel_capacity_explicit: Option<usize>,
    /// Explicit data-channel capacity override (takes priority over config)
    data_channel_capacity_explicit: Option<usize>,
    /// Explicit link keepalive override in seconds (takes priority over config)
    link_keepalive_secs_explicit: Option<u64>,
    /// Explicit auto-connect cap override (takes priority over config value).
    /// `0` disables runtime auto-connect; `N > 0` enables it capped at `N`.
    autoconnect_discovered_interfaces_explicit: Option<usize>,
    /// Instance name to connect to as a shared instance client.
    /// Mutually exclusive with share_instance.
    connect_instance_name: Option<String>,
    /// Whether the application event channel is constructed at all.
    /// Default true. Daemon-style nodes that have no application code
    /// to consume `NodeEvent`s should set this to false via
    /// `without_events()`.
    events_enabled: bool,
    /// Explicit discovery announcer job interval in seconds (Codeberg #107).
    /// Takes priority over the config value; fast tests lower it.
    discovery_job_interval_secs_explicit: Option<u64>,
    /// In-driver core processor (Codeberg #196), moved into the event loop by
    /// `ReticulumNode::start()`.
    core_processor: Option<Box<dyn CoreProcessor>>,
}

impl Default for ReticulumNodeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ReticulumNodeBuilder {
    /// Create a new builder with default settings
    pub fn new() -> Self {
        Self {
            core_builder: NodeCoreBuilder::new(),
            loaded_config: None,
            config_path: None,
            storage_path: None,
            interfaces: Vec::new(),
            rnode_channels: Vec::new(),
            corrupt_every: None,
            outbound_socket_hook: None,
            enable_transport_explicit: None,
            share_instance_explicit: None,
            instance_name_explicit: None,
            flush_interval_secs_explicit: None,
            control_channel_capacity_explicit: None,
            data_channel_capacity_explicit: None,
            link_keepalive_secs_explicit: None,
            autoconnect_discovered_interfaces_explicit: None,
            connect_instance_name: None,
            events_enabled: true,
            discovery_job_interval_secs_explicit: None,
            core_processor: None,
        }
    }

    /// Register an in-driver core processor (Codeberg #196).
    ///
    /// Inside the driver's tick the processor receives `&mut StdNodeCore` plus
    /// each `NodeEvent` the core produced, and returns a `TickOutput` the
    /// driver dispatches on its own send path — in the same tick, which is what
    /// lets `PacketProofRequested`, `LinkProofRequested` and
    /// `ResourceAdvertised` be answered rather than deferred. The events it
    /// sees are tapped ahead of the lossy application sink, so it observes
    /// `EventClass::Data` events the sink is entitled to drop.
    ///
    /// Read [`CoreProcessor`] before writing one: it runs with the core lock
    /// held, under a documented [`PROCESSOR_TICK_BUDGET`], and the shape of its
    /// methods is load-bearing rather than stylistic.
    ///
    /// Registration is builder-only on purpose, and that has two consequences
    /// worth knowing before you write one.
    ///
    /// * The event loop, and with it the bounded `action_dispatch_tx` a
    ///   processor must never block on, does not exist yet at this point.
    ///   Neither does the node — so a processor cannot be built holding a
    ///   handle to the node it will run inside, which is the deadlock
    ///   [`CoreProcessor`] warns about.
    /// * **A processor survives exactly one `start()`.** `start()` moves it
    ///   into the event loop; a `stop()` / `start()` cycle on the same node
    ///   therefore runs with no processor at all, and says nothing about it.
    ///   Nothing re-registers it, because re-registration would have to happen
    ///   while the loop owns it. If a node is restarted, build a new one.
    ///
    /// # Panics
    ///
    /// A panic inside either hook does not kill the node: the driver catches
    /// it, detaches the processor for good, and emits
    /// `NodeEvent::CoreProcessorPanicked` on the event stream. Whatever the
    /// processor was driving stops there — recovery means a new node.
    ///
    /// [`PROCESSOR_TICK_BUDGET`]: super::PROCESSOR_TICK_BUDGET
    ///
    /// # Example
    ///
    /// ```no_run
    /// use leviculum_core::node::NodeEvent;
    /// use leviculum_core::transport::TickOutput;
    /// use leviculum_std::driver::{CoreProcessor, ReticulumNodeBuilder, StdNodeCore};
    ///
    /// struct CountAnnounces {
    ///     seen: u64,
    /// }
    ///
    /// impl CoreProcessor for CountAnnounces {
    ///     fn on_event(&mut self, _core: &mut StdNodeCore, event: &NodeEvent) -> TickOutput {
    ///         if matches!(event, NodeEvent::AnnounceReceived { .. }) {
    ///             self.seen += 1;
    ///         }
    ///         TickOutput::empty()
    ///     }
    /// }
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let node = ReticulumNodeBuilder::new()
    ///     .core_processor(CountAnnounces { seen: 0 })
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn core_processor(mut self, processor: impl CoreProcessor) -> Self {
        self.core_processor = Some(Box::new(processor));
        self
    }

    /// Disable the application event channel.
    ///
    /// Use this for daemon-style nodes that have no application code
    /// to consume `NodeEvent`s (e.g. `lnsd`, where local clients are
    /// served via the shared-instance Unix socket and the RPC server
    /// reads directly from `NodeCore`). Forwarding (broadcasts,
    /// directed sends, local-client routing) is unaffected — it runs
    /// entirely on `output.actions`. Mirrors the `leviculum-nrf`
    /// daemon binaries, which never construct an event channel.
    ///
    /// After `without_events()`, `take_event_receiver()` returns `None`.
    pub fn without_events(mut self) -> Self {
        self.events_enabled = false;
        self
    }

    /// Set the identity for the node
    ///
    /// If not set, a random identity will be generated.
    pub fn identity(mut self, identity: Identity) -> Self {
        self.core_builder = self.core_builder.identity(identity);
        self
    }

    /// Set the proof strategy for the node
    pub fn proof_strategy(mut self, strategy: ProofStrategy) -> Self {
        self.core_builder = self.core_builder.proof_strategy(strategy);
        self
    }

    /// Set the resource receive-window adaptation policy (Codeberg #85).
    /// Default: [`WindowPolicy::Current`]. Endpoint tools read it from the
    /// `LEVICULUM_RESOURCE_WINDOW_POLICY` environment variable via
    /// [`crate::resource_policy::resource_window_policy_from_env`].
    pub fn resource_window_policy(mut self, policy: WindowPolicy) -> Self {
        self.core_builder = self.core_builder.resource_window_policy(policy);
        self
    }

    /// Use a pre-loaded configuration
    ///
    /// The builder will use this config for storage path, interface
    /// configurations, and transport settings. Takes priority over
    /// `config_file()`.
    pub fn config(mut self, config: Config) -> Self {
        self.loaded_config = Some(config);
        self
    }

    /// Load configuration from a file
    ///
    /// If set, the builder will attempt to load configuration from this path.
    /// Interface configurations from the file will be merged with any
    /// manually added interfaces. Ignored if `config()` was called.
    pub fn config_file(mut self, path: PathBuf) -> Self {
        self.config_path = Some(path);
        self
    }

    /// Set the storage path
    ///
    /// If not set, a default path will be used.
    pub fn storage_path(mut self, path: PathBuf) -> Self {
        self.storage_path = Some(path);
        self
    }

    /// Add a TCP client interface
    ///
    /// This connects to a remote Reticulum node as a client.
    pub fn add_tcp_client(mut self, addr: SocketAddr) -> Self {
        self.interfaces.push(InterfaceConfig {
            interface_type: "TCPClientInterface".to_string(),
            target_host: Some(addr.ip().to_string()),
            target_port: Some(addr.port()),
            ..Default::default()
        });
        self
    }

    /// Add a TCP client interface with a configured `bitrate` (Codeberg #93).
    /// Equivalent to a `[[TCPClientInterface]]` block that also sets `bitrate`
    /// (bits per second). The value feeds the announce bandwidth cap / timing
    /// and the effective bitrate reported via `interface_stats`, matching
    /// Python's `configured_bitrate` override.
    pub fn add_tcp_client_with_bitrate(mut self, addr: SocketAddr, bitrate: u64) -> Self {
        self.interfaces.push(InterfaceConfig {
            interface_type: "TCPClientInterface".to_string(),
            target_host: Some(addr.ip().to_string()),
            target_port: Some(addr.port()),
            bitrate: Some(bitrate),
            ..Default::default()
        });
        self
    }

    /// Add a TCP client interface with per-interface announce-rate limiting
    /// configured (Codeberg #92). Equivalent to a `[[TCPClientInterface]]`
    /// block that also sets `announce_rate_target/grace/penalty` (seconds /
    /// count / seconds). Used to drive config-driven rebroadcast rate limiting
    /// against a real Python peer in interop tests.
    pub fn add_tcp_client_with_announce_rate(
        mut self,
        addr: SocketAddr,
        announce_rate_target: u32,
        announce_rate_grace: u32,
        announce_rate_penalty: u32,
    ) -> Self {
        self.interfaces.push(InterfaceConfig {
            interface_type: "TCPClientInterface".to_string(),
            target_host: Some(addr.ip().to_string()),
            target_port: Some(addr.port()),
            announce_rate_target: Some(announce_rate_target),
            announce_rate_grace: Some(announce_rate_grace),
            announce_rate_penalty: Some(announce_rate_penalty),
            ..Default::default()
        });
        self
    }

    /// Add a TCP server interface
    ///
    /// This listens for incoming connections from other Reticulum nodes.
    pub fn add_tcp_server(mut self, addr: SocketAddr) -> Self {
        self.interfaces.push(InterfaceConfig {
            // A config file names its interfaces; the programmatic API does
            // not, so fall back to the type name the way an operator would
            // (the name shows up in `TCPServerInterface[<name>/<ip>:<port>]`).
            name: "TCP Server".to_string(),
            interface_type: "TCPServerInterface".to_string(),
            listen_ip: Some(addr.ip().to_string()),
            listen_port: Some(addr.port()),
            ..Default::default()
        });
        self
    }

    /// Add a discoverable TCP server interface (Codeberg #107).
    ///
    /// Equivalent to a `[[TCPServerInterface]]` block that sets `discoverable =
    /// yes` plus the discovery keys: the node's periodic
    /// [`InterfaceAnnouncer`](leviculum_core::discovery) self-advertises this
    /// endpoint on `rnstransport.discovery.interface`, so a Python `rnsd`
    /// discovers it autonomously. `reachable_on` is the advertised host
    /// (defaults to the listen IP); `announce_interval_secs` is the per-interface
    /// cadence (test-friendly seconds, no Python 5-minute floor); `encrypt`
    /// requests network-identity encryption (requires a configured network
    /// identity).
    pub fn add_discoverable_tcp_server(
        mut self,
        addr: SocketAddr,
        name: impl Into<String>,
        announce_interval_secs: u64,
        encrypt: bool,
    ) -> Self {
        let ip = addr.ip().to_string();
        let name = name.into();
        self.interfaces.push(InterfaceConfig {
            name: name.clone(),
            interface_type: "TCPServerInterface".to_string(),
            listen_ip: Some(ip.clone()),
            listen_port: Some(addr.port()),
            discoverable: true,
            discovery_name: Some(name),
            reachable_on: Some(ip),
            discovery_encrypt: encrypt,
            discovery_announce_interval_secs: Some(announce_interval_secs),
            ..Default::default()
        });
        self
    }

    /// Override the discovery announcer job interval in seconds (Codeberg #107,
    /// Python `InterfaceAnnouncer.JOB_INTERVAL`, default 60). Takes priority over
    /// the config value; fast tests lower it so the announcer fires promptly.
    pub fn discovery_announce_job_interval_secs(mut self, secs: u64) -> Self {
        self.discovery_job_interval_secs_explicit = Some(secs);
        self
    }

    /// Add a UDP interface
    ///
    /// Binds to `listen_addr` for incoming datagrams and sends outgoing
    /// datagrams to `forward_addr`. No framing, each datagram is one packet.
    pub fn add_udp_interface(mut self, listen_addr: SocketAddr, forward_addr: SocketAddr) -> Self {
        self.interfaces.push(InterfaceConfig {
            interface_type: "UDPInterface".to_string(),
            listen_ip: Some(listen_addr.ip().to_string()),
            listen_port: Some(listen_addr.port()),
            forward_ip: Some(forward_addr.ip().to_string()),
            forward_port: Some(forward_addr.port()),
            ..Default::default()
        });
        self
    }

    /// Add a UDP interface that forwards each outgoing datagram to several
    /// addresses (Rust-only extension; Python's UDPInterface supports one).
    /// Stored as a comma-separated `forward_ip` with per-entry ports.
    pub fn add_udp_multi_forward_interface(
        mut self,
        listen_addr: SocketAddr,
        forward_addrs: &[SocketAddr],
    ) -> Self {
        let forward_ip = forward_addrs
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(",");
        self.interfaces.push(InterfaceConfig {
            interface_type: "UDPInterface".to_string(),
            listen_ip: Some(listen_addr.ip().to_string()),
            listen_port: Some(listen_addr.port()),
            forward_ip: Some(forward_ip),
            forward_port: None,
            ..Default::default()
        });
        self
    }

    /// Add an RNode (LoRa) interface programmatically, the equivalent of an
    /// `[[RNode]]` config block. The six parameters are the required radio
    /// settings; optional tuning (airtime limits, flow control, buffer size)
    /// keeps the driver defaults. Use a config file for the optional knobs.
    pub fn add_rnode_interface(
        mut self,
        port: String,
        frequency: u64,
        bandwidth: u32,
        spreading_factor: u8,
        coding_rate: u8,
        tx_power: i8,
    ) -> Self {
        self.interfaces.push(InterfaceConfig {
            interface_type: "RNodeInterface".to_string(),
            port: Some(port),
            frequency: Some(frequency),
            bandwidth: Some(bandwidth),
            spreading_factor: Some(spreading_factor),
            coding_rate: Some(coding_rate),
            tx_power: Some(tx_power),
            ..Default::default()
        });
        self
    }

    /// Add an RNode interface over a host-supplied byte channel rather than a
    /// serial port path.
    ///
    /// `factory` is called once per (re)connection to open a fresh duplex
    /// channel to the RNode firmware (see [`RNodeChannelFactory`]). This is the
    /// path for phone-attached radios — Android USB host / BLE GATT, iOS BLE —
    /// where the process never sees `/dev/ttyACM*`. The radio lifecycle
    /// (detect → configure → online → reconnect-on-drop) is identical to
    /// [`add_rnode_interface`](Self::add_rnode_interface); only the transport
    /// differs. Optional tuning (airtime limits, flow control, buffer size)
    /// uses the driver defaults.
    pub fn add_rnode_channel_interface(
        mut self,
        factory: Arc<dyn RNodeChannelFactory>,
        frequency: u64,
        bandwidth: u32,
        spreading_factor: u8,
        coding_rate: u8,
        tx_power: i8,
    ) -> Self {
        self.rnode_channels.push(RNodeChannelConfig {
            factory,
            frequency: frequency as u32,
            bandwidth,
            tx_power: tx_power as u8,
            sf: spreading_factor,
            cr: coding_rate,
            st_alock: None,
            lt_alock: None,
            flow_control: false,
            buffer_size: RNODE_DEFAULT_BUFFER_SIZE,
        });
        self
    }

    /// Add a serial interface programmatically, the equivalent of a
    /// `[[Serial]]` config block. KISS framing over a raw serial port.
    pub fn add_serial_interface(
        mut self,
        port: String,
        speed: u32,
        databits: u8,
        parity: String,
        stopbits: u8,
    ) -> Self {
        self.interfaces.push(InterfaceConfig {
            interface_type: "SerialInterface".to_string(),
            port: Some(port),
            speed: Some(speed),
            databits: Some(databits),
            parity: Some(parity),
            stopbits: Some(stopbits),
            ..Default::default()
        });
        self
    }

    /// Add a KISS interface programmatically, the equivalent of a
    /// `[[...]] type = KISSInterface` config block. KISS TNC framing over a
    /// serial port. TNC parameters (preamble/txtail/persistence/slottime) and
    /// beacon keys default to the Python `KISSInterface` values when unset here;
    /// tune them via the config file if needed.
    pub fn add_kiss_interface(
        mut self,
        port: String,
        speed: u32,
        databits: u8,
        parity: String,
        stopbits: u8,
        flow_control: bool,
    ) -> Self {
        self.interfaces.push(InterfaceConfig {
            interface_type: "KISSInterface".to_string(),
            port: Some(port),
            speed: Some(speed),
            databits: Some(databits),
            parity: Some(parity),
            stopbits: Some(stopbits),
            flow_control: Some(flow_control),
            ..Default::default()
        });
        self
    }

    /// Add an AX.25 KISS interface programmatically, the equivalent of a
    /// `[[...]] type = AX25KISSInterface` config block. A plain KISS interface
    /// wrapped in an AX.25 UI-frame header keyed on `callsign`/`ssid` (Python
    /// validates callsign length 3-6 and ssid 0-15). The tocall is the fixed
    /// `APZRNS-0` Python uses, so frames interoperate byte-for-byte with a
    /// Python `AX25KISSInterface`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_ax25_kiss_interface(
        mut self,
        port: String,
        callsign: String,
        ssid: u8,
        speed: u32,
        databits: u8,
        parity: String,
        stopbits: u8,
        flow_control: bool,
    ) -> Self {
        self.interfaces.push(InterfaceConfig {
            interface_type: "AX25KISSInterface".to_string(),
            port: Some(port),
            callsign: Some(callsign),
            ssid: Some(ssid),
            speed: Some(speed),
            databits: Some(databits),
            parity: Some(parity),
            stopbits: Some(stopbits),
            flow_control: Some(flow_control),
            ..Default::default()
        });
        self
    }

    /// Add a PipeInterface that bridges packets through an external program.
    ///
    /// `command` is a shell-style command line spawned as a subprocess;
    /// HDLC-framed packets are written to its stdin and read from its stdout.
    /// `respawn_delay_secs` sets how long to wait before relaunching the child
    /// after it exits (`None` → the 5s Python default). Mirrors Python
    /// Reticulum's `PipeInterface`.
    pub fn add_pipe_interface(mut self, command: String, respawn_delay_secs: Option<f64>) -> Self {
        self.interfaces.push(InterfaceConfig {
            interface_type: "PipeInterface".to_string(),
            command: Some(command),
            respawn_delay: respawn_delay_secs,
            ..Default::default()
        });
        self
    }

    /// Set the Reticulum propagation mode (Codeberg #91) on the most recently
    /// added interface. The string uses the same spellings as the config file
    /// (`full`, `gateway`/`gw`, `access_point`/`ap`, `pointtopoint`/`ptp`,
    /// `roaming`, `boundary`); it is resolved to an `InterfaceMode` at
    /// registration. No-op if no interface has been added yet.
    pub fn interface_mode(mut self, mode: &str) -> Self {
        if let Some(last) = self.interfaces.last_mut() {
            last.mode = Some(mode.to_string());
        }
        self
    }

    /// Add an AutoInterface with default configuration
    ///
    /// Zero-configuration LAN discovery via IPv6 multicast.
    /// Peers are discovered automatically on the local network.
    pub fn add_auto_interface(self) -> Self {
        self.add_auto_interface_with_config(
            crate::interfaces::auto_interface::AutoInterfaceConfig::default(),
        )
    }

    /// Add an AutoInterface with custom configuration
    pub fn add_auto_interface_with_config(
        mut self,
        config: crate::interfaces::auto_interface::AutoInterfaceConfig,
    ) -> Self {
        self.interfaces.push(InterfaceConfig {
            interface_type: "AutoInterface".to_string(),
            group_id: Some(String::from_utf8_lossy(&config.group_id).to_string()),
            discovery_scope: Some(config.discovery_scope),
            discovery_port: Some(config.discovery_port),
            data_port: Some(config.data_port),
            devices: config.allowed_devices,
            ignored_devices: config.ignored_devices,
            multicast_loopback: Some(config.multicast_loopback),
            ..Default::default()
        });
        self
    }

    /// Enable fault injection: corrupt ~1 byte per N bytes on TCP write
    pub fn corrupt_every(mut self, n: Option<u64>) -> Self {
        self.corrupt_every = n;
        self
    }

    /// Register a hook invoked with each outbound TCP connect socket before it
    /// dials, so an embedder can apply host-level socket policy. See
    /// [`crate::socket_hook::OutboundSocketHook`].
    pub fn outbound_socket_hook(mut self, hook: crate::socket_hook::OutboundSocketHook) -> Self {
        self.outbound_socket_hook = Some(hook);
        self
    }

    /// Enable or disable transport mode
    ///
    /// When enabled, this node will forward packets between interfaces.
    /// If not called, the value from the loaded config is used (default: true).
    pub fn enable_transport(mut self, enabled: bool) -> Self {
        self.enable_transport_explicit = Some(enabled);
        self
    }

    /// Override the link keepalive interval (seconds) for every link.
    ///
    /// Takes priority over the config `keepalive_interval`. When unset, the
    /// RTT-derived default is used. Shrinking it also shrinks the stale-link
    /// timeout, which is what makes stale/recovery testable in seconds.
    pub fn link_keepalive(mut self, secs: u64) -> Self {
        self.link_keepalive_secs_explicit = Some(secs);
        self
    }

    /// Enable or disable shared instance (local IPC socket).
    ///
    /// When enabled, the daemon listens on an abstract Unix socket for
    /// local client programs. If not called, uses the config value (default: false).
    pub fn share_instance(mut self, enabled: bool) -> Self {
        self.share_instance_explicit = Some(enabled);
        self
    }

    /// Set the instance name for the shared instance socket.
    ///
    /// The abstract socket path will be `\0rns/{name}`. Default: "default".
    pub fn instance_name(mut self, name: String) -> Self {
        self.instance_name_explicit = Some(name);
        self
    }

    /// Connect to an existing shared instance daemon as a client.
    ///
    /// The node will connect to `\0rns/{instance_name}` and route all
    /// traffic through the daemon. No config-file interfaces (TCP, UDP,
    /// Auto, RNode) will be loaded, the daemon connection is the only
    /// interface.
    ///
    /// Should be used with `enable_transport(false)`.
    /// Mutually exclusive with `share_instance(true)`.
    pub fn connect_to_shared_instance(mut self, instance_name: impl Into<String>) -> Self {
        self.connect_instance_name = Some(instance_name.into());
        self
    }

    /// Set the interval between periodic storage flushes in seconds.
    ///
    /// Crash protection only, normal shutdown flushes via the signal
    /// handler. If not called, the value from the loaded config is used
    /// (default: 3600 seconds).
    pub fn flush_interval_secs(mut self, secs: u64) -> Self {
        self.flush_interval_secs_explicit = Some(secs);
        self
    }

    /// Enable runtime auto-connect of discovered interfaces (Codeberg #32).
    ///
    /// `max` is both the on/off flag and the concurrency cap, matching Python's
    /// `autoconnect_discovered_interfaces`: `0` disables it (the default);
    /// `N > 0` enables it and auto-connects at most `N` discovered
    /// (Backbone/TCP) endpoints at a time. Overrides the loaded config value.
    pub fn autoconnect_discovered_interfaces(mut self, max: usize) -> Self {
        self.autoconnect_discovered_interfaces_explicit = Some(max);
        self
    }

    /// Set the capacity of the lossless control-plane event channel
    /// (Codeberg #71).
    ///
    /// Control events (announces, paths, link/resource lifecycle) are
    /// delivered losslessly until this bounded channel fills; overflow is
    /// then surfaced via `NodeEvent::ControlPlaneOverflow`. If not called,
    /// the loaded config value is used (default:
    /// [`DEFAULT_CONTROL_CHANNEL_CAPACITY`](crate::config::DEFAULT_CONTROL_CHANNEL_CAPACITY)).
    /// Servers under heavy announce load should raise it.
    pub fn control_channel_capacity(mut self, capacity: usize) -> Self {
        self.control_channel_capacity_explicit = Some(capacity);
        self
    }

    /// Set the capacity of the droppable data-plane event channel
    /// (Codeberg #71).
    ///
    /// Data events (single-packet delivery and confirmations) drop silently
    /// when this bounded channel is full — normal backpressure. If not
    /// called, the loaded config value is used (default:
    /// [`DEFAULT_DATA_CHANNEL_CAPACITY`](crate::config::DEFAULT_DATA_CHANNEL_CAPACITY)).
    pub fn data_channel_capacity(mut self, capacity: usize) -> Self {
        self.data_channel_capacity_explicit = Some(capacity);
        self
    }

    /// Set path expiry duration in seconds.
    ///
    /// Paths not refreshed within this duration will be removed.
    /// Default is 7 days (604800 seconds).
    pub fn path_expiry_secs(mut self, secs: u64) -> Self {
        self.core_builder = self.core_builder.path_expiry_secs(secs);
        self
    }

    /// Resolve config: pre-loaded > loaded from path > default
    fn resolve_config(&self) -> Result<Config, Error> {
        if let Some(ref config) = self.loaded_config {
            return Ok(config.clone());
        }
        if let Some(ref path) = self.config_path {
            if path.exists() {
                return Config::load(path);
            }
        }
        Ok(Config::default())
    }

    /// Build the ReticulumNode synchronously
    ///
    /// Same as `build()` but does not require an async context.
    /// Useful when constructing a node outside of an async runtime.
    pub fn build_sync(mut self) -> Result<ReticulumNode, Error> {
        // Resolve config: pre-loaded > loaded from path > default
        let config = self.resolve_config()?;

        // Apply enable_transport: explicit override > config value
        let enable_transport = self
            .enable_transport_explicit
            .unwrap_or(config.reticulum.enable_transport);

        // Apply shared_instance: explicit override > config value
        let share_instance = self
            .share_instance_explicit
            .unwrap_or(config.reticulum.shared_instance);
        let instance_name = self
            .instance_name_explicit
            .unwrap_or_else(|| config.reticulum.instance_name.clone());

        // Apply flush interval: explicit override > config value
        let flush_interval_secs = self
            .flush_interval_secs_explicit
            .unwrap_or(config.reticulum.flush_interval_secs);

        // Apply auto-connect cap: explicit override > config value (Codeberg #32).
        let autoconnect_max = self
            .autoconnect_discovered_interfaces_explicit
            .unwrap_or(config.reticulum.autoconnect_discovered_interfaces);

        // Apply event-channel capacities: explicit override > config value
        // (Codeberg #71).
        let control_channel_capacity = self
            .control_channel_capacity_explicit
            .unwrap_or(config.reticulum.control_channel_capacity);
        let data_channel_capacity = self
            .data_channel_capacity_explicit
            .unwrap_or(config.reticulum.data_channel_capacity);

        // Determine storage path
        let storage_path = self
            .storage_path
            .or_else(|| config.reticulum.storage_path.clone())
            .unwrap_or_else(|| Config::default_config_dir().join("storage"));

        // Network identity for a private (encrypted) discovery network (Codeberg
        // #32, sub-task d). When configured, load (or generate + persist, like
        // Python Reticulum.py:521-542) the shared 64-byte identity so encrypted
        // discovery announces can be decrypted on receive. A load failure
        // downgrades to plaintext discovery rather than aborting the daemon.
        let discovery_network_identity =
            config.reticulum.network_identity.clone().and_then(|path| {
                let resolved = resolve_network_identity_path(&path, &storage_path);
                match load_or_create_network_identity(&resolved) {
                    Ok(id) => {
                        tracing::info!(
                            "Discovery network identity loaded: {} ({})",
                            hex_short(id.hash()),
                            resolved.display()
                        );
                        Some(Arc::new(id))
                    }
                    Err(e) => {
                        tracing::error!(
                            "Failed to load discovery network identity from {}: {e}; \
                                 falling back to plaintext discovery",
                            resolved.display()
                        );
                        None
                    }
                }
            });

        // Storage::new() loads persistent data (known_destinations, packet_hashlist)
        // into its inner MemoryStorage automatically.
        let storage = Storage::new(&storage_path)?;
        let clock = SystemClock::new();
        // Align the LoRa airtime bucket's anchor with Transport's
        // SystemClock so `try_send_prioritized`'s `last_update_ms` and
        // `push_next_slot_ms`'s `now_ms` share a frame.
        crate::interfaces::init_clock_anchor(clock.start_instant());

        // Merge interface configs from file. The parsed map is keyed by
        // section name, so flattening it drops the ordering the config file
        // had; sorting by name puts it back on a defined footing (the
        // reported inventory is ordered by interface id, which follows this
        // order) instead of leaving it to HashMap iteration, which differs
        // run to run. Deviation from Python, which keeps file order
        // (configobj is ordered); pinned in `interfaces_are_ordered_by_name`.
        let mut interfaces = config.interfaces.into_values().collect::<Vec<_>>();
        interfaces.sort_by(|a, b| a.name.cmp(&b.name));
        interfaces.extend(self.interfaces);

        // Channel-backed RNode interfaces (factory closures, not file-config).
        let rnode_channels = self.rnode_channels;

        // Load or generate transport identity via FileIdentityStore
        let mut id_store = crate::file_identity_store::FileIdentityStore::new(&storage_path);
        let core_builder = if self.core_builder.identity_ref().is_none() {
            use leviculum_core::identity_store::IdentityStore;
            let identity = match id_store.load() {
                Ok(Some(id)) => {
                    tracing::info!("Loaded transport identity: {}", hex_short(id.hash()));
                    tracing::info!(event = "IDENTITY", node = %hex_full(id.hash()));
                    id
                }
                Ok(None) => {
                    let id = Identity::generate(&mut rand_core::OsRng);
                    id_store
                        .save(&id)
                        .map_err(|e| Error::Storage(format!("failed to save identity: {e}")))?;
                    tracing::info!("Generated new transport identity: {}", hex_short(id.hash()));
                    tracing::info!(event = "IDENTITY", node = %hex_full(id.hash()));
                    id
                }
                Err(e) => return Err(Error::Storage(format!("failed to load identity: {e}"))),
            };
            self.core_builder.identity(identity)
        } else {
            // Explicit identity, write to file so Python tools can read it.
            // A save failure is not fatal here (the identity is caller-supplied,
            // not generated), but warn so it is not lost silently the way the
            // generate branch would surface it.
            use leviculum_core::identity_store::IdentityStore;
            if let Some(id) = self.core_builder.identity_ref() {
                if let Err(e) = id_store.save(id) {
                    tracing::warn!("failed to persist explicit identity: {e}");
                }
            }
            self.core_builder
        };
        // Apply link keepalive override: explicit > config value.
        let link_keepalive_secs = self
            .link_keepalive_secs_explicit
            .map(Some)
            .unwrap_or(config.reticulum.keepalive_interval);

        // Remote management (Codeberg #86): parse the ACL hex hashes into
        // 16-byte identity hashes, dropping malformed entries with a warning
        // (Python raises; we tolerate so one bad line does not down the daemon).
        let remote_management_allowed: Vec<[u8; leviculum_core::constants::TRUNCATED_HASHBYTES]> =
            config
                .reticulum
                .remote_management_allowed
                .iter()
                .filter_map(|hex| match parse_identity_hash16(hex) {
                    Some(h) => Some(h),
                    None => {
                        tracing::warn!(
                            "ignoring invalid remote_management_allowed identity hash: {hex:?}"
                        );
                        None
                    }
                })
                .collect();

        let core_builder = core_builder
            .enable_transport(enable_transport)
            .link_keepalive(link_keepalive_secs)
            .respond_to_probes(config.reticulum.respond_to_probes)
            .remote_management(
                config.reticulum.remote_management_enabled,
                remote_management_allowed,
            );

        // Build NodeCore (consumes storage, persistent data already loaded)
        let node_core = core_builder.build(rand_core::OsRng, clock, storage);

        let mut node = ReticulumNode::new(
            node_core,
            interfaces,
            self.corrupt_every,
            self.events_enabled,
            flush_interval_secs,
            control_channel_capacity,
            data_channel_capacity,
        );
        node.outbound_socket_hook = self.outbound_socket_hook.clone();
        // Capture the configured shared-instance TCP-loopback ports
        // (`shared_instance_port` / `instance_control_port`, Codeberg #112) for
        // the AF_INET bind path. Unconditional: the AF_UNIX path never reads
        // them (it keys by `instance_name`), so this is a no-op there, matching
        // Python, which only honours the ports when AF_UNIX is unavailable or
        // `shared_instance_type = tcp`.
        crate::interfaces::local::set_loopback_ports(
            config.reticulum.shared_instance_port,
            config.reticulum.instance_control_port,
        );
        if share_instance {
            node.set_share_instance(instance_name);
        }
        if let Some(ref name) = self.connect_instance_name {
            node.set_connect_instance(name.clone());
        }
        node.set_storage_path(storage_path.clone());
        node.set_rnode_channels(rnode_channels);
        node.set_autoconnect_max(autoconnect_max);
        node.set_discovery_network_identity(discovery_network_identity);
        if let Some(processor) = self.core_processor.take() {
            node.set_core_processor(processor);
        }
        node.set_discovery_job_interval_secs(
            self.discovery_job_interval_secs_explicit
                .unwrap_or(config.reticulum.discovery_job_interval_secs),
        );

        Ok(node)
    }

    /// Build the ReticulumNode
    ///
    /// This creates the node, initializes storage, and prepares interfaces.
    /// The node is not yet running after this call - use `start()` to begin.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Configuration file loading fails
    /// - Storage initialization fails
    /// - Identity generation fails
    pub async fn build(self) -> Result<ReticulumNode, Error> {
        // Async version delegates to build_sync (no async operations needed here)
        self.build_sync()
    }
}

/// Format the first 8 bytes of a hash as hex for logging
fn hex_short(hash: &[u8]) -> String {
    use std::fmt::Write;
    let n = hash.len().min(8);
    hash[..n]
        .iter()
        .fold(String::with_capacity(n * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Parse a 32-hex-character (16-byte) identity hash. Returns `None` for any
/// wrong length or non-hex input (matches Python's ACL length check,
/// Reticulum.py:555-558, but tolerantly instead of raising).
fn parse_identity_hash16(
    hex: &str,
) -> Option<[u8; leviculum_core::constants::TRUNCATED_HASHBYTES]> {
    let hex = hex.trim();
    if hex.len() != leviculum_core::constants::TRUNCATED_HASHBYTES * 2 {
        return None;
    }
    let mut out = [0u8; leviculum_core::constants::TRUNCATED_HASHBYTES];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Resolve a configured `network_identity` path (Codeberg #32, sub-task d).
///
/// Expands a leading `~/` against `$HOME` (Python `os.path.expanduser`); a
/// still-relative path is resolved against the storage directory so the
/// identity lives alongside the node's other state.
fn resolve_network_identity_path(
    path: &std::path::Path,
    storage_path: &std::path::Path,
) -> PathBuf {
    let expanded = match path.strip_prefix("~") {
        Ok(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => path.to_path_buf(),
        },
        Err(_) => path.to_path_buf(),
    };
    if expanded.is_absolute() {
        expanded
    } else {
        storage_path.join(expanded)
    }
}

/// Load a network identity from a raw 64-byte file, generating and persisting a
/// fresh one when the file is absent (Python Reticulum.py:521-542).
fn load_or_create_network_identity(path: &std::path::Path) -> std::io::Result<Identity> {
    use leviculum_core::constants::IDENTITY_KEY_SIZE;
    match std::fs::read(path) {
        Ok(bytes) if bytes.len() == IDENTITY_KEY_SIZE => Identity::from_private_key_bytes(&bytes)
            .map_err(|e| std::io::Error::other(format!("{e:?}"))),
        Ok(bytes) => Err(std::io::Error::other(format!(
            "network identity file has wrong size: {} (expected {IDENTITY_KEY_SIZE})",
            bytes.len()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let id = Identity::generate(&mut rand_core::OsRng);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let key_bytes = id
                .private_key_bytes()
                .map_err(|e| std::io::Error::other(format!("{e:?}")))?;
            std::fs::write(path, key_bytes)?;
            tracing::info!(
                "Network identity generated and persisted to {}",
                path.display()
            );
            Ok(id)
        }
        Err(e) => Err(e),
    }
}

/// Format a full hash as hex for logging
fn hex_full(hash: &[u8]) -> String {
    use std::fmt::Write;
    hash.iter()
        .fold(String::with_capacity(hash.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_default() {
        let builder = ReticulumNodeBuilder::new();
        assert!(builder.config_path.is_none());
        assert!(builder.interfaces.is_empty());
    }

    #[test]
    fn test_builder_with_identity() {
        let identity = Identity::generate(&mut rand_core::OsRng);
        let _builder = ReticulumNodeBuilder::new().identity(identity);
        // Identity is consumed by NodeCoreBuilder, verified by the build test
    }

    #[test]
    fn test_builder_add_tcp_client() {
        let addr: SocketAddr = "127.0.0.1:4242".parse().unwrap();
        let builder = ReticulumNodeBuilder::new().add_tcp_client(addr);
        assert_eq!(builder.interfaces.len(), 1);
        assert_eq!(builder.interfaces[0].interface_type, "TCPClientInterface");
    }

    #[test]
    fn test_builder_add_udp_interface() {
        let listen: SocketAddr = "0.0.0.0:4242".parse().unwrap();
        let forward: SocketAddr = "192.168.1.255:4242".parse().unwrap();
        let builder = ReticulumNodeBuilder::new().add_udp_interface(listen, forward);
        assert_eq!(builder.interfaces.len(), 1);
        assert_eq!(builder.interfaces[0].interface_type, "UDPInterface");
        assert_eq!(builder.interfaces[0].listen_ip, Some("0.0.0.0".to_string()));
        assert_eq!(builder.interfaces[0].listen_port, Some(4242));
        assert_eq!(
            builder.interfaces[0].forward_ip,
            Some("192.168.1.255".to_string())
        );
        assert_eq!(builder.interfaces[0].forward_port, Some(4242));
    }

    #[test]
    fn test_builder_add_udp_multi_forward_interface() {
        let listen: SocketAddr = "0.0.0.0:4242".parse().unwrap();
        let forward_1: SocketAddr = "192.168.1.255:4242".parse().unwrap();
        let forward_2: SocketAddr = "10.0.0.255:5000".parse().unwrap();
        let builder = ReticulumNodeBuilder::new()
            .add_udp_multi_forward_interface(listen, &[forward_1, forward_2]);
        assert_eq!(builder.interfaces.len(), 1);
        assert_eq!(builder.interfaces[0].interface_type, "UDPInterface");
        assert_eq!(
            builder.interfaces[0].forward_ip,
            Some("192.168.1.255:4242,10.0.0.255:5000".to_string())
        );
        assert_eq!(builder.interfaces[0].forward_port, None);
    }

    #[test]
    fn test_builder_enable_transport_explicit_override() {
        let builder = ReticulumNodeBuilder::new().enable_transport(false);
        assert_eq!(builder.enable_transport_explicit, Some(false));
    }

    #[test]
    fn test_builder_flush_interval_explicit_override() {
        let builder = ReticulumNodeBuilder::new().flush_interval_secs(120);
        assert_eq!(builder.flush_interval_secs_explicit, Some(120));
    }

    #[test]
    fn test_builder_channel_capacity_explicit_override() {
        let builder = ReticulumNodeBuilder::new()
            .control_channel_capacity(1024)
            .data_channel_capacity(512);
        assert_eq!(builder.control_channel_capacity_explicit, Some(1024));
        assert_eq!(builder.data_channel_capacity_explicit, Some(512));
    }

    #[test]
    fn test_builder_channel_capacity_defaults_from_config() {
        let td = tempfile::tempdir().expect("tempdir");
        let node = ReticulumNodeBuilder::new()
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync failed");
        assert_eq!(
            node.control_channel_capacity,
            crate::config::DEFAULT_CONTROL_CHANNEL_CAPACITY,
            "default config should use the conservative control capacity"
        );
    }

    #[test]
    fn test_builder_channel_capacity_explicit_overrides_config() {
        let td = tempfile::tempdir().expect("tempdir");
        let mut config = Config::default();
        config.reticulum.control_channel_capacity = 64;
        let node = ReticulumNodeBuilder::new()
            .config(config)
            .control_channel_capacity(2048)
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync failed");
        assert_eq!(
            node.control_channel_capacity, 2048,
            "explicit control capacity should override config value"
        );
    }

    #[test]
    fn test_builder_flush_interval_defaults_from_config() {
        let td = tempfile::tempdir().expect("tempdir");
        let node = ReticulumNodeBuilder::new()
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync failed");
        assert_eq!(
            node.flush_interval_secs,
            crate::config::DEFAULT_FLUSH_INTERVAL_SECS,
            "default config should keep the 3600 s flush interval"
        );
    }

    #[test]
    fn test_builder_flush_interval_config_value_used() {
        let td = tempfile::tempdir().expect("tempdir");
        let mut config = Config::default();
        config.reticulum.flush_interval_secs = 900;
        let node = ReticulumNodeBuilder::new()
            .config(config)
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync failed");
        assert_eq!(node.flush_interval_secs, 900);
    }

    #[test]
    fn test_builder_flush_interval_explicit_overrides_config() {
        let td = tempfile::tempdir().expect("tempdir");
        let mut config = Config::default();
        config.reticulum.flush_interval_secs = 900;
        let node = ReticulumNodeBuilder::new()
            .config(config)
            .flush_interval_secs(60)
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync failed");
        assert_eq!(
            node.flush_interval_secs, 60,
            "explicit flush_interval_secs should override config value"
        );
    }

    #[test]
    fn test_builder_defaults_transport_enabled_from_config() {
        // No explicit enable_transport call, should use config default (true)
        let td = tempfile::tempdir().expect("tempdir");
        let node = ReticulumNodeBuilder::new()
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync failed");
        assert!(
            node.is_transport_enabled(),
            "default config should enable transport"
        );
    }

    #[test]
    fn test_builder_explicit_false_overrides_config() {
        let td = tempfile::tempdir().expect("tempdir");
        let node = ReticulumNodeBuilder::new()
            .enable_transport(false)
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync failed");
        assert!(
            !node.is_transport_enabled(),
            "explicit false should override config default"
        );
    }

    #[test]
    fn test_builder_config_false_respected() {
        let td = tempfile::tempdir().expect("tempdir");
        let mut config = Config::default();
        config.reticulum.enable_transport = false;
        let node = ReticulumNodeBuilder::new()
            .config(config)
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync failed");
        assert!(
            !node.is_transport_enabled(),
            "config with enable_transport=false should be respected"
        );
    }

    #[test]
    fn test_builder_explicit_true_overrides_config_false() {
        let td = tempfile::tempdir().expect("tempdir");
        let mut config = Config::default();
        config.reticulum.enable_transport = false;
        let node = ReticulumNodeBuilder::new()
            .config(config)
            .enable_transport(true)
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync failed");
        assert!(
            node.is_transport_enabled(),
            "explicit true should override config false"
        );
    }

    fn temp_storage_path() -> PathBuf {
        std::env::temp_dir().join(format!("reticulum_test_builder_{}", std::process::id()))
    }

    #[test]
    fn test_identity_round_trip() {
        use leviculum_core::identity_store::IdentityStore;
        let path = temp_storage_path().join("rt");
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();

        let mut store = crate::file_identity_store::FileIdentityStore::new(&path);
        let id = Identity::generate(&mut rand_core::OsRng);
        store.save(&id).unwrap();

        let loaded = store.load().unwrap().expect("should load saved identity");
        assert_eq!(id.hash(), loaded.hash());

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_first_run_creates_identity_file() {
        use leviculum_core::identity_store::IdentityStore;
        let path = temp_storage_path().join("first_run");
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();

        let mut store = crate::file_identity_store::FileIdentityStore::new(&path);
        assert!(store.load().unwrap().is_none());

        let id = Identity::generate(&mut rand_core::OsRng);
        store.save(&id).unwrap();

        let file_path = path.join("transport_identity");
        assert!(file_path.exists(), "identity file should be created");
        let bytes = std::fs::read(&file_path).unwrap();
        assert_eq!(bytes.len(), 64);

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_second_run_loads_same_identity() {
        use leviculum_core::identity_store::IdentityStore;
        let path = temp_storage_path().join("second_run");
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();

        let mut store = crate::file_identity_store::FileIdentityStore::new(&path);
        let id1 = Identity::generate(&mut rand_core::OsRng);
        store.save(&id1).unwrap();

        let id2 = store.load().unwrap().expect("should load saved identity");
        assert_eq!(id1.hash(), id2.hash());

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_explicit_identity_overrides_persistence() {
        let path = temp_storage_path().join("explicit_id");
        let _ = std::fs::remove_dir_all(&path);

        let explicit_id = Identity::generate(&mut rand_core::OsRng);
        let explicit_hash = *explicit_id.hash();

        let node = ReticulumNodeBuilder::new()
            .identity(explicit_id)
            .storage_path(path.clone())
            .build_sync()
            .expect("build_sync failed");

        assert_eq!(node.identity_hash(), explicit_hash);

        let id_file = path.join("transport_identity");
        assert!(
            id_file.exists(),
            "explicit identity should write transport_identity for Python tool compatibility"
        );
        let bytes = std::fs::read(&id_file).unwrap();
        let loaded = Identity::from_private_key_bytes(&bytes).unwrap();
        assert_eq!(loaded.hash(), &explicit_hash);

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_build_sync_persists_identity() {
        let path = temp_storage_path().join("build_persist");
        let _ = std::fs::remove_dir_all(&path);

        let node1 = ReticulumNodeBuilder::new()
            .storage_path(path.clone())
            .build_sync()
            .expect("first build_sync failed");
        let hash1 = node1.identity_hash();

        let node2 = ReticulumNodeBuilder::new()
            .storage_path(path.clone())
            .build_sync()
            .expect("second build_sync failed");
        let hash2 = node2.identity_hash();

        assert_eq!(hash1, hash2, "identity should persist across builds");

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_wrong_size_identity_file_is_a_hard_error() {
        use leviculum_core::identity_store::IdentityStore;
        let path = temp_storage_path().join("wrong_size");
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();

        // A truncated identity file: the shape a power cut mid-write leaves.
        let file = path.join("transport_identity");
        std::fs::write(&file, b"too_short").unwrap();

        let mut store = crate::file_identity_store::FileIdentityStore::new(&path);
        let Err(err) = store.load() else {
            panic!("a short identity file must refuse, not report absence");
        };
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("transport_identity"),
            "the error must name the unreadable file, got: {err}"
        );

        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"too_short",
            "the damaged file must survive so a backup can be reconciled"
        );

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_truncated_identity_refuses_build_instead_of_rotating() {
        // The data-loss path end to end: builder must not mint a fresh
        // identity over a damaged file (#273).
        let path = temp_storage_path().join("truncated_build");
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();

        let good = Identity::generate(&mut rand_core::OsRng);
        let good_bytes = good.private_key_bytes().unwrap();
        let file = path.join("transport_identity");
        // Half a key, as an interrupted write would leave it.
        std::fs::write(&file, &good_bytes[..32]).unwrap();

        let Err(err) = ReticulumNodeBuilder::new()
            .storage_path(path.clone())
            .build_sync()
        else {
            panic!("build must refuse on a damaged identity file");
        };
        assert!(
            err.to_string().contains("transport_identity"),
            "the operator must be told which file is unreadable, got: {err}"
        );

        assert_eq!(
            std::fs::read(&file).unwrap().len(),
            32,
            "the damaged file must not be overwritten by a new identity"
        );

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_save_leaves_no_temp_file_behind() {
        use leviculum_core::identity_store::IdentityStore;
        let path = temp_storage_path().join("durable_save");
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();

        let mut store = crate::file_identity_store::FileIdentityStore::new(&path);
        store
            .save(&Identity::generate(&mut rand_core::OsRng))
            .unwrap();

        assert!(path.join("transport_identity").exists());
        assert!(
            !path.join("transport_identity.tmp").exists(),
            "the temp file must be renamed into place, not left next to it"
        );

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_python_transport_identity_compat() {
        // Read the actual Python rnsd transport_identity file if present
        let python_path = dirs::home_dir().map(|h| h.join(".reticulum/storage/transport_identity"));
        let Some(path) = python_path else { return };
        if !path.exists() {
            return; // Skip if Python rnsd hasn't been run on this machine
        }

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            bytes.len(),
            64,
            "Python transport_identity should be 64 bytes"
        );

        let id = Identity::from_private_key_bytes(&bytes).unwrap();
        assert!(id.has_private_keys());
        // Verify the identity produces a valid hash (16 bytes)
        assert_eq!(id.hash().len(), 16);
        // Verify it can sign and verify
        let msg = b"test message";
        let sig = id.sign(msg).unwrap();
        assert!(id.verify(msg, &sig).unwrap());
    }

    #[test]
    fn test_network_identity_loaded_and_persisted() {
        // Codeberg #32 sub-task d: a configured network_identity is resolved,
        // generated-on-missing, persisted, and wired onto the node so the
        // event loop can decrypt encrypted discovery announces.
        let td = tempfile::tempdir().expect("tempdir");
        let mut config = Config::default();
        // Relative path resolves against the storage directory.
        config.reticulum.network_identity = Some(PathBuf::from("discovery/network_identity"));
        let node = ReticulumNodeBuilder::new()
            .config(config)
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync with network_identity failed");

        assert!(
            node.discovery_network_identity.is_some(),
            "network identity should be loaded onto the node"
        );

        // The 64-byte identity file was created and reloads to the same hash.
        let id_path = td.path().join("discovery/network_identity");
        let bytes = std::fs::read(&id_path).expect("network identity persisted");
        assert_eq!(bytes.len(), 64, "network identity is a raw 64-byte file");
        let reloaded = Identity::from_private_key_bytes(&bytes).unwrap();
        assert_eq!(
            reloaded.hash(),
            node.discovery_network_identity.as_ref().unwrap().hash(),
            "persisted identity matches the loaded one"
        );
    }

    #[test]
    fn test_no_network_identity_leaves_node_plaintext() {
        let td = tempfile::tempdir().expect("tempdir");
        let node = ReticulumNodeBuilder::new()
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync failed");
        assert!(
            node.discovery_network_identity.is_none(),
            "no network identity configured => plaintext discovery path"
        );
    }

    #[test]
    fn test_respond_to_probes_registers_destination() {
        let td = tempfile::tempdir().expect("tempdir");
        let mut config = Config::default();
        config.reticulum.respond_to_probes = true;
        let node = ReticulumNodeBuilder::new()
            .config(config)
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync with respond_to_probes failed");
        // Core should have the probe destination hash
        let inner = node.inner.lock().unwrap();
        assert!(
            inner.probe_dest_hash().is_some(),
            "probe_dest_hash should be set when respond_to_probes is enabled"
        );
    }

    #[test]
    fn test_respond_to_probes_disabled_by_default() {
        let td = tempfile::tempdir().expect("tempdir");
        let node = ReticulumNodeBuilder::new()
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync failed");
        let inner = node.inner.lock().unwrap();
        assert!(
            inner.probe_dest_hash().is_none(),
            "probe_dest_hash should be None when respond_to_probes is disabled"
        );
    }

    // Reuse the dirs module from config.rs for home dir lookup in tests
    mod dirs {
        use std::path::PathBuf;
        pub(super) fn home_dir() -> Option<PathBuf> {
            std::env::var_os("HOME").map(PathBuf::from)
        }
    }
}
