//! RNode (single LoRa transceiver) interface builder.

use crate::config::InterfaceConfig;
use crate::error::Error;
use leviculum_core::transport::InterfaceId;

use super::super::resolve_lt_alock;
use super::{Built, InterfaceBuildCtx};

pub(super) fn build(
    idx: usize,
    config: &InterfaceConfig,
    ctx: &InterfaceBuildCtx<'_>,
) -> Result<Built, Error> {
    let port_path = config
        .port
        .as_ref()
        .ok_or_else(|| Error::Config("RNodeInterface requires port".to_string()))?
        .clone();
    let frequency: u32 = config
        .frequency
        .ok_or_else(|| Error::Config("RNodeInterface requires frequency".to_string()))
        .and_then(|f| {
            u32::try_from(f)
                .map_err(|_| Error::Config(format!("frequency {} exceeds u32 range", f)))
        })?;
    let bandwidth = config
        .bandwidth
        .ok_or_else(|| Error::Config("RNodeInterface requires bandwidth".to_string()))?;
    let sf = config
        .spreading_factor
        .ok_or_else(|| Error::Config("RNodeInterface requires spreading_factor".to_string()))?;
    let cr = config
        .coding_rate
        .ok_or_else(|| Error::Config("RNodeInterface requires coding_rate".to_string()))?;
    // A carrier whose occupied bandwidth touches one of the narrowband alarm
    // bands between the ERC 70-03 wideband sub-bands is warned about loudly
    // and then honoured. The reason it is not refused is project policy: no
    // radio configuration is ever refused for a radio-regulatory reason,
    // because the operator — not this software — carries the legal
    // responsibility for compliant operation, and the same carrier is lawful
    // under a licence, in another region, or in a shielded chamber. WARN, not
    // debug: a warning behind a filter is the silent substitution this policy
    // exists to prevent.
    if let Some(gap) = leviculum_core::rnode::erp_band_gap(u64::from(frequency), bandwidth) {
        tracing::warn!(
            "RNodeInterface: frequency {} Hz with bandwidth {} Hz overlaps the {} band, \
             where ERC 70-03 permits only <= 25 kHz channel spacing; \
             choose a centre frequency whose signal fits a listed sub-band",
            frequency,
            bandwidth,
            gap
        );
    }

    // Absent `txpower` asks for the board maximum capped by the lawful ERP
    // limit for the frequency. Resolved before the value loses its `Option`,
    // because `None` and `Some(0)` must stay distinguishable: an explicit
    // `txpower = 0` still means 0. The resolution logs its outcome (capped,
    // board maximum, or no citable limit) itself.
    let requested_tx_power =
        leviculum_core::rnode::resolve_tx_power(config.tx_power, u64::from(frequency));
    let tx_power_derived = config.tx_power.is_none();
    let tx_power: u8 = requested_tx_power.try_into().map_err(|_| {
        Error::Config(format!(
            "tx_power {} out of range (0-37)",
            requested_tx_power
        ))
    })?;

    leviculum_core::rnode::validate_config(frequency, bandwidth, tx_power, sf, cr)
        .map_err(|e| Error::Config(format!("RNodeInterface: {}", e)))?;

    let st_alock = config.airtime_limit_short.map(|p| (p * 100.0) as u16);
    let lt_alock = resolve_lt_alock(config.airtime_limit_long, frequency);
    let flow_control = config.flow_control.unwrap_or(false);
    let buffer_size = config
        .buffer_size
        .unwrap_or(crate::interfaces::rnode::RNODE_DEFAULT_BUFFER_SIZE);

    let iface_name = format!("rnode_{}", idx);
    let id = InterfaceId(idx);

    let handle = crate::interfaces::rnode::spawn_rnode_interface(
        crate::interfaces::rnode::RNodeInterfaceConfig {
            id,
            name: iface_name,
            port_path: port_path.clone(),
            frequency,
            bandwidth,
            tx_power,
            tx_power_derived,
            sf,
            cr,
            st_alock,
            lt_alock,
            flow_control,
            buffer_size,
            reconnect_notify: Some(ctx.reconnect_tx.clone()),
            test_drop_direct_ingress: config.test_drop_direct_ingress,
        },
    );

    tracing::info!(
        "RNode interface on {} (freq={} Hz, sf={}, bw={} Hz, cr={}, txp={} dBm)",
        port_path,
        frequency,
        sf,
        bandwidth,
        cr,
        tx_power,
    );
    Ok(Built::Handles(vec![handle]))
}
