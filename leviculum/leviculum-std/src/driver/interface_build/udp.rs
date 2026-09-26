//! UDP interface builder.

use std::net::SocketAddr;

use crate::config::InterfaceConfig;
use crate::error::Error;
use crate::interfaces::udp::spawn_udp_interface;
use leviculum_core::transport::InterfaceId;

use super::{Built, InterfaceBuildCtx};

pub(super) fn build(
    idx: usize,
    config: &InterfaceConfig,
    _ctx: &InterfaceBuildCtx<'_>,
) -> Result<Built, Error> {
    // A configured `device` supplies the NIC's IPv4 broadcast address for
    // whichever of listen_ip / forward_ip is left unset (Codeberg #3,
    // UDPInterface.py:82-86). Explicit keys win over it.
    let device_broadcast = match config.device.as_deref() {
        Some(device) => Some(
            crate::interfaces::netdevice::resolve_if_broadcast(device)
                .map_err(|e| Error::Config(format!("UDPInterface device \"{}\": {}", device, e)))?
                .to_string(),
        ),
        None => None,
    };

    // A single `port` key fills both the bind port and the forward port when
    // neither is set explicitly, matching rnsd (UDPInterface.py:68-72). On a
    // UDP block `config.port` (the shared string slot) is a port number, and
    // nothing else consumes it here, so a config brought over from a working
    // rnsd setup that names only `port` now starts instead of being rejected
    // (Codeberg #279). An explicit `listen_port` / `forward_port` wins.
    let shared_port: Option<u16> = config
        .port
        .as_deref()
        .and_then(|s| s.trim().parse::<u16>().ok());

    let listen_ip = config
        .listen_ip
        .as_deref()
        .or(device_broadcast.as_deref())
        .unwrap_or("0.0.0.0");
    let listen_port = config
        .listen_port
        .or(shared_port)
        .ok_or_else(|| Error::Config("UDPInterface requires listen_port or port".to_string()))?;
    let forward_ip = config
        .forward_ip
        .as_deref()
        .or(device_broadcast.as_deref())
        .ok_or_else(|| Error::Config("UDPInterface requires forward_ip".to_string()))?;
    let forward_port = config.forward_port.or(shared_port);

    let listen_addr: SocketAddr = format!("{}:{}", listen_ip, listen_port)
        .parse()
        .map_err(|e| Error::Config(format!("UDPInterface invalid listen address: {}", e)))?;
    // `forward_ip` may hold several comma-separated entries (Rust-only
    // extension); each outgoing datagram goes to every one of them. Each
    // entry is an address or a hostname (Codeberg #148) — hostnames are
    // resolved by the interface at runtime, so a name that does not resolve
    // is an interface-level error here, not a config error, matching rnsd
    // (Python defers the lookup to sendto).
    let forward_targets = crate::interfaces::udp::parse_forward_addrs(forward_ip, forward_port)
        .map_err(|e| match e {
            crate::interfaces::udp::ForwardAddrError::MissingPort => {
                Error::Config("UDPInterface requires forward_port".to_string())
            }
            crate::interfaces::udp::ForwardAddrError::Invalid(msg) => {
                Error::Config(format!("UDPInterface invalid forward address: {}", msg))
            }
        })?;

    let iface_name = format!("udp_{}", idx);
    let id = InterfaceId(idx);
    let forward_desc = forward_targets
        .iter()
        .map(|t| t.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let handle = spawn_udp_interface(id, iface_name, listen_addr, forward_targets)?;
    tracing::info!(
        "UDP interface listening on {}, forwarding to {}",
        listen_addr,
        forward_desc
    );
    Ok(Built::Handles(vec![handle]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InterfaceConfig;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    struct CtxOwner {
        next_id: Arc<AtomicUsize>,
        new_iface_tx: mpsc::Sender<crate::interfaces::InterfaceHandle>,
        reconnect_tx: mpsc::Sender<InterfaceId>,
        tunnel_notify_tx: mpsc::Sender<InterfaceId>,
        peer_event_tx: mpsc::Sender<(InterfaceId, crate::interfaces::PeerEvent)>,
        inventory: crate::interfaces::inventory::SharedInventory,
    }

    impl CtxOwner {
        fn new() -> Self {
            let (new_iface_tx, _) = mpsc::channel(4);
            let (reconnect_tx, _) = mpsc::channel(4);
            let (tunnel_notify_tx, _) = mpsc::channel(4);
            let (peer_event_tx, _) = mpsc::channel(4);
            Self {
                next_id: Arc::new(AtomicUsize::new(100)),
                new_iface_tx,
                reconnect_tx,
                tunnel_notify_tx,
                peer_event_tx,
                inventory: crate::interfaces::inventory::InterfaceInventory::shared(),
            }
        }

        fn ctx(&self) -> InterfaceBuildCtx<'_> {
            InterfaceBuildCtx {
                next_id: &self.next_id,
                new_iface_tx: &self.new_iface_tx,
                reconnect_tx: &self.reconnect_tx,
                tunnel_notify_tx: &self.tunnel_notify_tx,
                peer_event_tx: &self.peer_event_tx,
                corrupt_every: None,
                storage_path: None,
                outbound_socket_hook: None,
                inventory: self.inventory.clone(),
                transport_enabled: false,
                identity_hash: [0x5A; 16],
            }
        }
    }

    /// A single `port` key fills both the bind and the forward port, so a UDP
    /// block that names only `port` (plus a forward address) builds instead of
    /// being rejected for a missing `listen_port` (Codeberg #279, rnsd
    /// UDPInterface.py:68-72). Port 0 keeps the bind ephemeral so the test
    /// never races a fixed port.
    #[tokio::test]
    async fn single_port_key_fills_bind_and_forward_ports() {
        let owner = CtxOwner::new();
        let config = InterfaceConfig {
            interface_type: "UDPInterface".to_string(),
            port: Some("0".to_string()),
            forward_ip: Some("127.0.0.1".to_string()),
            ..Default::default()
        };
        let built =
            build(0, &config, &owner.ctx()).expect("a UDP block with only `port` must build");
        let Built::Handles(handles) = built else {
            panic!("UDP builds one handle");
        };
        assert_eq!(handles.len(), 1);
    }

    /// With neither `port` nor `listen_port`, the bind port is genuinely
    /// missing and the error names both spellings.
    #[test]
    fn missing_both_port_spellings_is_a_named_error() {
        let owner = CtxOwner::new();
        let config = InterfaceConfig {
            interface_type: "UDPInterface".to_string(),
            forward_ip: Some("127.0.0.1".to_string()),
            forward_port: Some(4242),
            ..Default::default()
        };
        let err = build(0, &config, &owner.ctx())
            .err()
            .expect("no bind port must not build");
        assert!(err.to_string().contains("listen_port or port"), "{err}");
    }
}
