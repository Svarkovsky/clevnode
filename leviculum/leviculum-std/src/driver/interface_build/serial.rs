//! Serial (KISS-less async serial / LoRa-serial) interface builder.

use crate::config::InterfaceConfig;
use crate::error::Error;
use leviculum_core::transport::InterfaceId;

use super::{Built, InterfaceBuildCtx};

pub(super) fn build(
    idx: usize,
    config: &InterfaceConfig,
    ctx: &InterfaceBuildCtx<'_>,
) -> Result<Built, Error> {
    let port_path = config
        .port
        .as_ref()
        .ok_or_else(|| Error::Config("SerialInterface requires port".to_string()))?
        .clone();
    let speed = config.speed.unwrap_or(9600);
    let data_bits = crate::interfaces::serial::parse_data_bits(config.databits.unwrap_or(8));
    let parity = crate::interfaces::serial::parse_parity(config.parity.as_deref().unwrap_or("N"));
    let stop_bits = crate::interfaces::serial::parse_stop_bits(config.stopbits.unwrap_or(1));
    let buffer_size = config
        .buffer_size
        .unwrap_or(crate::interfaces::serial::SERIAL_DEFAULT_BUFFER_SIZE);

    let iface_name = format!("serial_{}", idx);
    let id = InterfaceId(idx);

    // A `frequency` makes this a LoRa modem — the LNode path — and a carrier
    // occupying one of the ERC 70-03 narrowband alarm bands is warned about
    // loudly and then honoured, exactly as on the RNode builders: no radio
    // configuration is ever refused for a radio-regulatory reason, because the
    // operator carries the legal responsibility for compliant operation and the
    // same carrier is lawful under a licence, in another region, or in a
    // shielded chamber. WARN, not debug: a warning behind a filter is the
    // silent substitution this policy exists to prevent. Checked before
    // `serial_radio_config` resolves any radio parameter; the bandwidth default
    // mirrors `serial_radio_config`'s.
    if let Some(frequency) = config.frequency {
        let bandwidth = config.bandwidth.unwrap_or(125_000);
        if let Some(gap) = leviculum_core::rnode::erp_band_gap(frequency, bandwidth) {
            tracing::warn!(
                "SerialInterface: frequency {} Hz with bandwidth {} Hz overlaps the {} band, \
                 where ERC 70-03 permits only <= 25 kHz channel spacing; \
                 choose a centre frequency whose signal fits a listed sub-band",
                frequency,
                bandwidth,
                gap
            );
        }
    }

    // A `preamble_symbols` pin above the measured SX127x RX ceiling keys a
    // preamble no SX127x peer can receive (Codeberg #315). Warn, never
    // refuse: an SX126x-only mesh may do this legitimately.
    if let Some(warning) = crate::interfaces::serial::preamble_ceiling_warning(&iface_name, config)
    {
        tracing::warn!("{warning}");
    }

    let radio_config = crate::interfaces::serial::serial_radio_config(config);

    // A LoRa modem block (one that names a `frequency`) has its radio
    // parameters validated exactly like the RNode builders, so a value that
    // cannot work is refused here with the interface named rather than
    // surviving to the airtime arithmetic where a `bandwidth = 0` divides by
    // zero and kills the daemon at startup (Codeberg #274). A plain serial
    // pipe names no frequency and skips this.
    if let Some(rc) = &radio_config {
        let frequency = u32::try_from(rc.frequency).map_err(|_| {
            Error::Config(format!(
                "SerialInterface: frequency {} exceeds u32 range",
                rc.frequency
            ))
        })?;
        let tx_power = u8::try_from(rc.tx_power).map_err(|_| {
            Error::Config(format!(
                "SerialInterface: tx_power {} out of range (0-{})",
                rc.tx_power,
                leviculum_core::rnode::MAX_TX_POWER
            ))
        })?;
        leviculum_core::rnode::validate_config(
            frequency,
            rc.bandwidth,
            tx_power,
            rc.spreading_factor,
            rc.coding_rate,
        )
        .map_err(|e| Error::Config(format!("SerialInterface: {}", e)))?;
    }

    let mut handle = crate::interfaces::serial::spawn_serial_interface(
        crate::interfaces::serial::SerialInterfaceConfig {
            id,
            name: iface_name.clone(),
            port: port_path.clone(),
            speed,
            data_bits,
            parity,
            stop_bits,
            buffer_size,
            reconnect_notify: Some(ctx.reconnect_tx.clone()),
            radio_config,
            test_drop_direct_ingress: config.test_drop_direct_ingress,
        },
    );
    handle.info.bitrate = Some(speed);

    tracing::info!("Serial interface on {} (speed={} baud)", port_path, speed,);
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

    /// A LoRa SerialInterface block with `bandwidth = 0` is refused at build
    /// time with a named config error rather than dividing by zero in the
    /// airtime arithmetic and panicking at daemon startup (Codeberg #274).
    #[test]
    fn zero_bandwidth_lora_config_is_a_named_config_error() {
        let owner = CtxOwner::new();
        let config = InterfaceConfig {
            interface_type: "SerialInterface".to_string(),
            port: Some("/dev/null".to_string()),
            frequency: Some(868_000_000),
            bandwidth: Some(0),
            ..Default::default()
        };
        let err = build(0, &config, &owner.ctx())
            .err()
            .expect("bandwidth 0 must not build");
        let msg = err.to_string();
        assert!(
            msg.contains("SerialInterface") && msg.contains("bandwidth"),
            "expected a named bandwidth config error, got: {msg}"
        );
    }
}
