//! RNodeMulti interface builder: one serial port fanned out into several LoRa
//! transceivers, each an independent logical interface.

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
        .ok_or_else(|| Error::Config("RNodeMultiInterface requires port".to_string()))?
        .clone();

    let parent_name = format!("rnode_multi_{}", idx);
    let flow_control = config.flow_control.unwrap_or(false);
    let buffer_size = config
        .buffer_size
        .unwrap_or(crate::interfaces::rnode::RNODE_DEFAULT_BUFFER_SIZE);

    let mut subs = Vec::new();
    // Each enabled [[[subinterface]]] becomes its own logical interface; the
    // first reuses the section index, the rest draw fresh ids.
    let mut first_id = Some(InterfaceId(idx));
    for sub in config.subinterfaces.iter().filter(|s| s.enabled) {
        let vport = sub.vport.ok_or_else(|| {
            Error::Config(format!(
                "RNodeMultiInterface subinterface '{}' requires vport",
                sub.name
            ))
        })?;
        if vport as u16 >= leviculum_core::rnode::MAX_SUBINTERFACES as u16 {
            return Err(Error::Config(format!(
                "RNodeMultiInterface subinterface '{}' vport {} out of range (0-{})",
                sub.name,
                vport,
                leviculum_core::rnode::MAX_SUBINTERFACES - 1
            )));
        }
        let frequency: u32 = sub
            .frequency
            .ok_or_else(|| {
                Error::Config(format!(
                    "RNodeMultiInterface subinterface '{}' requires frequency",
                    sub.name
                ))
            })
            .and_then(|f| {
                u32::try_from(f)
                    .map_err(|_| Error::Config(format!("frequency {} exceeds u32 range", f)))
            })?;
        let bandwidth = sub.bandwidth.ok_or_else(|| {
            Error::Config(format!(
                "RNodeMultiInterface subinterface '{}' requires bandwidth",
                sub.name
            ))
        })?;
        let sf = sub.spreading_factor.ok_or_else(|| {
            Error::Config(format!(
                "RNodeMultiInterface subinterface '{}' requires spreadingfactor",
                sub.name
            ))
        })?;
        let cr = sub.coding_rate.ok_or_else(|| {
            Error::Config(format!(
                "RNodeMultiInterface subinterface '{}' requires codingrate",
                sub.name
            ))
        })?;
        // A carrier occupying one of the ERC 70-03 narrowband alarm bands is
        // warned about and then honoured — the regulatory judgement is the
        // operator's. See the single-interface builder for the policy.
        if let Some(gap) = leviculum_core::rnode::erp_band_gap(u64::from(frequency), bandwidth) {
            tracing::warn!(
                "RNodeMultiInterface subinterface '{}': frequency {} Hz with bandwidth {} Hz \
                 overlaps the {} band, where ERC 70-03 permits only <= 25 kHz channel spacing; \
                 choose a centre frequency whose signal fits a listed sub-band",
                sub.name,
                frequency,
                bandwidth,
                gap
            );
        }

        // Absent `txpower` asks for the board maximum capped by the lawful
        // ERP limit for the frequency; an explicit `txpower = 0` still means
        // 0. See the single-interface builder.
        let requested_tx_power =
            leviculum_core::rnode::resolve_tx_power(sub.tx_power, u64::from(frequency));
        let tx_power_derived = sub.tx_power.is_none();
        let tx_power: u8 = requested_tx_power.try_into().map_err(|_| {
            Error::Config(format!(
                "tx_power {} out of range (0-37)",
                requested_tx_power
            ))
        })?;

        leviculum_core::rnode::validate_config(frequency, bandwidth, tx_power, sf, cr).map_err(
            |e| {
                Error::Config(format!(
                    "RNodeMultiInterface subinterface '{}': {}",
                    sub.name, e
                ))
            },
        )?;

        let id = first_id.take().unwrap_or_else(|| {
            InterfaceId(
                ctx.next_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            )
        });
        subs.push(crate::interfaces::rnode::RNodeSubinterfaceParams {
            id,
            name: format!("{}[{}]", parent_name, sub.name),
            vport,
            frequency,
            bandwidth,
            tx_power,
            tx_power_derived,
            sf,
            cr,
            st_alock: sub.airtime_limit_short.map(|p| (p * 100.0) as u16),
            lt_alock: resolve_lt_alock(sub.airtime_limit_long, frequency),
            outgoing: sub.outgoing,
        });
    }

    if subs.is_empty() {
        return Err(Error::Config(format!(
            "RNodeMultiInterface '{}' has no enabled subinterfaces",
            parent_name
        )));
    }

    let vport_count = subs.len();
    let handles = crate::interfaces::rnode::spawn_rnode_multi_interface(
        crate::interfaces::rnode::RNodeMultiInterfaceConfig {
            name: parent_name.clone(),
            port_path: port_path.clone(),
            subinterfaces: subs,
            flow_control,
            buffer_size,
            reconnect_notify: Some(ctx.reconnect_tx.clone()),
        },
    );
    tracing::info!(
        "RNodeMulti interface on {} ({} vport subinterfaces)",
        port_path,
        vport_count
    );
    Ok(Built::Handles(handles))
}
