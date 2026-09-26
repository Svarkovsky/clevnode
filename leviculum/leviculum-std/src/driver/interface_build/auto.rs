//! AutoInterface orchestrator builder.

use crate::config::InterfaceConfig;
use crate::error::Error;
use crate::interfaces::auto_interface::orchestrator::spawn_auto_interface;
use crate::interfaces::auto_interface::AutoInterfaceConfig;

use super::super::AutoPeerCount;
use super::{Built, InterfaceBuildCtx};

pub(super) fn build(
    config: &InterfaceConfig,
    ctx: &InterfaceBuildCtx<'_>,
    auto_peer_count: &AutoPeerCount,
) -> Result<Built, Error> {
    let discovery_port = config
        .discovery_port
        .unwrap_or(crate::interfaces::auto_interface::DEFAULT_DISCOVERY_PORT);
    let data_port = config
        .data_port
        .unwrap_or(crate::interfaces::auto_interface::DEFAULT_DATA_PORT);

    let auto_config = AutoInterfaceConfig {
        group_id: config
            .group_id
            .as_deref()
            .map(|s| s.as_bytes().to_vec())
            .unwrap_or_else(|| crate::interfaces::auto_interface::DEFAULT_GROUP_ID.to_vec()),
        discovery_port,
        data_port,
        discovery_scope: config
            .discovery_scope
            .clone()
            .unwrap_or_else(|| "link".to_string()),
        allowed_devices: config.devices.clone(),
        ignored_devices: config.ignored_devices.clone(),
        multicast_loopback: config.multicast_loopback.unwrap_or(true),
    };
    let peer_count_rx =
        spawn_auto_interface(ctx.next_id.clone(), ctx.new_iface_tx.clone(), auto_config);
    auto_peer_count.push(peer_count_rx);
    tracing::info!(
        "AutoInterface: starting orchestrator (discovery_port={}, data_port={})",
        discovery_port,
        data_port
    );
    Ok(Built::SelfManaged)
}
