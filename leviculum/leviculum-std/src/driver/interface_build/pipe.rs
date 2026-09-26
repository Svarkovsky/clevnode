//! Pipe (subprocess) interface builder.

use std::time::Duration;

use crate::config::InterfaceConfig;
use crate::error::Error;
use leviculum_core::transport::InterfaceId;

use super::{Built, InterfaceBuildCtx};

pub(super) fn build(
    idx: usize,
    config: &InterfaceConfig,
    ctx: &InterfaceBuildCtx<'_>,
) -> Result<Built, Error> {
    let command = config
        .command
        .as_ref()
        .ok_or_else(|| Error::Config("PipeInterface requires command".to_string()))?
        .clone();
    let respawn_delay = config
        .respawn_delay
        .filter(|d| d.is_finite() && *d >= 0.0)
        .map(Duration::from_secs_f64)
        .unwrap_or(crate::interfaces::pipe::PIPE_DEFAULT_RESPAWN_DELAY);
    let buffer_size = config
        .buffer_size
        .unwrap_or(crate::interfaces::pipe::PIPE_DEFAULT_BUFFER_SIZE);

    let iface_name = format!("pipe_{}", idx);
    let id = InterfaceId(idx);

    let handle = crate::interfaces::pipe::spawn_pipe_interface(
        crate::interfaces::pipe::PipeInterfaceConfig {
            id,
            name: iface_name,
            command: command.clone(),
            respawn_delay,
            buffer_size,
            reconnect_notify: Some(ctx.reconnect_tx.clone()),
            shutdown: None,
        },
    );

    tracing::info!("Pipe interface (command: {})", command);
    Ok(Built::Handles(vec![handle]))
}
