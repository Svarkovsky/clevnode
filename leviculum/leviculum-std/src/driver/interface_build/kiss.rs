//! KISS / AX25-KISS interface builder.

use crate::config::InterfaceConfig;
use crate::error::Error;
use leviculum_core::transport::InterfaceId;

use super::{Built, InterfaceBuildCtx};

pub(super) fn build(
    idx: usize,
    config: &InterfaceConfig,
    ctx: &InterfaceBuildCtx<'_>,
) -> Result<Built, Error> {
    let is_ax25 = config.interface_type == "AX25KISSInterface";
    let port_path = config
        .port
        .as_ref()
        .ok_or_else(|| Error::Config(format!("{} requires port", config.interface_type)))?
        .clone();

    // AX25KISSInterface adds an AX.25 UI-frame header keyed on a source
    // callsign/SSID (Python __init__ validates callsign length 3-6 and ssid
    // 0-15). Build + validate it here; a plain KISSInterface has none.
    let ax25 = if is_ax25 {
        let callsign = config
            .callsign
            .as_ref()
            .ok_or_else(|| Error::Config("AX25KISSInterface requires callsign".to_string()))?
            .to_uppercase();
        let ssid = config
            .ssid
            .ok_or_else(|| Error::Config("AX25KISSInterface requires ssid".to_string()))?;
        let addressing =
            leviculum_core::framing::ax25::Ax25Addressing::new(callsign.as_bytes(), ssid).map_err(
                |e| {
                    Error::Config(format!(
                        "AX25KISSInterface invalid AX.25 addressing \
                         (callsign '{}', ssid {}): {:?}",
                        callsign, ssid, e
                    ))
                },
            )?;
        Some(addressing)
    } else {
        None
    };
    // Python KISSInterface defaults: speed 9600, 8-N-1.
    let speed = config.speed.unwrap_or(9600);
    let data_bits = crate::interfaces::serial::parse_data_bits(config.databits.unwrap_or(8));
    let parity = crate::interfaces::serial::parse_parity(config.parity.as_deref().unwrap_or("N"));
    let stop_bits = crate::interfaces::serial::parse_stop_bits(config.stopbits.unwrap_or(1));
    let buffer_size = config
        .buffer_size
        .unwrap_or(crate::interfaces::kiss::KISS_DEFAULT_BUFFER_SIZE);

    let iface_name = if is_ax25 {
        format!("ax25kiss_{}", idx)
    } else {
        format!("kiss_{}", idx)
    };
    let id = InterfaceId(idx);

    let mut handle = crate::interfaces::kiss::spawn_kiss_interface(
        crate::interfaces::kiss::KissInterfaceConfig {
            id,
            name: iface_name.clone(),
            port: port_path.clone(),
            speed,
            data_bits,
            parity,
            stop_bits,
            preamble_ms: config
                .preamble
                .unwrap_or(crate::interfaces::kiss::DEFAULT_PREAMBLE_MS),
            txtail_ms: config
                .txtail
                .unwrap_or(crate::interfaces::kiss::DEFAULT_TXTAIL_MS),
            persistence: config
                .persistence
                .unwrap_or(crate::interfaces::kiss::DEFAULT_PERSISTENCE),
            slottime_ms: config
                .slottime
                .unwrap_or(crate::interfaces::kiss::DEFAULT_SLOTTIME_MS),
            flow_control: config.flow_control.unwrap_or(false),
            ax25,
            buffer_size,
            reconnect_notify: Some(ctx.reconnect_tx.clone()),
        },
    );
    handle.info.bitrate = Some(speed);

    tracing::info!(
        "{} interface on {} (speed={} baud)",
        config.interface_type,
        port_path,
        speed
    );
    if config.id_interval.is_some() || config.id_callsign.is_some() {
        tracing::warn!(
            "KISS interface {}: beacon identification (id_interval/id_callsign) \
             is configured but not yet transmitted (Codeberg #96 gap)",
            iface_name
        );
    }
    Ok(Built::Handles(vec![handle]))
}
