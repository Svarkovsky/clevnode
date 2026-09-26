//! I2P interface builder: an optional inbound endpoint plus one outbound
//! sub-interface per configured peer, all registered through `new_iface_tx`.

use std::time::Duration;

use crate::config::InterfaceConfig;
use crate::error::Error;
use crate::interfaces::i2p::{
    spawn_i2p_client, spawn_i2p_server, I2pClientConfig, I2pServerConfig, I2P_DEFAULT_BUFFER_SIZE,
    I2P_DEFAULT_RECONNECT_WAIT,
};
use leviculum_core::transport::InterfaceId;

use super::super::build_ifac_config;
use super::{Built, InterfaceBuildCtx};

pub(super) fn build(
    idx: usize,
    config: &InterfaceConfig,
    ctx: &InterfaceBuildCtx<'_>,
) -> Result<Built, Error> {
    // SAM bridge address: honour the I2P_SAM_ADDRESS env var (i2plib
    // `get_sam_address`), else the default 7656.
    let sam_address = std::env::var("I2P_SAM_ADDRESS")
        .unwrap_or_else(|_| crate::interfaces::i2p::sam::DEFAULT_SAM_ADDRESS.to_string());
    let buffer_size = config.buffer_size.unwrap_or(I2P_DEFAULT_BUFFER_SIZE);
    let reconnect_wait = config
        .reconnect_interval_secs
        .map(Duration::from_secs)
        .unwrap_or(I2P_DEFAULT_RECONNECT_WAIT);
    let ifac = build_ifac_config(config);
    // Codeberg #189: resolve the entry's ingress control once; every
    // sub-interface it spawns (accepted or dialled) inherits it, as in the
    // reference where a spawned I2P interface takes `self.ingress_control`
    // from the one parent (I2PInterface.py:951). A `connectable` entry takes
    // the listener default, a peers-only entry the dial-out one.
    let ingress_control = config.resolve_ingress_control();
    let storage_root = ctx
        .storage_path
        .clone()
        .unwrap_or_else(|| crate::config::Config::default_config_dir().join("storage"));

    // Server endpoint (accepts inbound I2P connections), spawning one
    // sub-interface per peer via new_iface_tx.
    if config.connectable.unwrap_or(false) {
        let keyfile = storage_root
            .join("i2p")
            .join(format!("i2p_iface_{}.i2p", idx));
        spawn_i2p_server(I2pServerConfig {
            sam_address: sam_address.clone(),
            keyfile,
            buffer_size,
            name_prefix: format!("i2p_{}", idx),
            reconnect_wait,
            next_id: ctx.next_id.clone(),
            new_interface_tx: ctx.new_iface_tx.clone(),
            ifac: ifac.clone(),
            ingress_control,
            outbound_socket_hook: ctx.outbound_socket_hook.clone(),
        });
        tracing::info!("I2P connectable endpoint (interface {})", idx);
    }

    // Outbound client sub-interface per configured peer. Routed through
    // new_iface_tx (like server-accepted connections) so each gets a unique id
    // with IFAC and hw_mtu applied uniformly by the registration branch. The
    // event loop is not consuming yet, so handles buffer in the channel until
    // it starts.
    if let Some(peers) = &config.peers {
        for peer in peers {
            let name = format!("i2p_{}_to_{}", idx, peer);
            // Reserve the registration slot BEFORE spawning (L-0063): a
            // spawned client whose handle cannot be delivered would keep
            // building SAM tunnels the driver can neither see nor tear
            // down. No slot, no task.
            let permit = match ctx.new_iface_tx.try_reserve() {
                Ok(permit) => permit,
                Err(e) => {
                    tracing::error!(
                        "NOT spawning I2P peer interface {}: no registration slot ({})",
                        name,
                        e
                    );
                    continue;
                }
            };
            let id = InterfaceId(
                ctx.next_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            );
            let handle = spawn_i2p_client(I2pClientConfig {
                id,
                name,
                sam_address: sam_address.clone(),
                peer: peer.clone(),
                buffer_size,
                reconnect_wait,
                ifac: ifac.clone(),
                reconnect_notify: Some(ctx.reconnect_tx.clone()),
                ingress_control,
                outbound_socket_hook: ctx.outbound_socket_hook.clone(),
            });
            permit.send(handle);
            tracing::info!("I2P client peer {} -> {}", idx, peer);
        }
    }

    if !config.connectable.unwrap_or(false) && config.peers.is_none() {
        tracing::warn!(
            "I2PInterface {} has neither `connectable = yes` nor `peers`; nothing to do",
            idx
        );
    }
    Ok(Built::SelfManaged)
}
