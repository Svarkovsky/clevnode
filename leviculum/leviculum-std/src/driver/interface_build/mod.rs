//! Per-entry interface construction, shared by [`initialize_interfaces`] at
//! startup and [`spawn_interface`] at runtime so a type is wired once for both.
//! Each interface family lives in its own submodule; this module holds the
//! shared context and the type dispatch.
//!
//! [`initialize_interfaces`]: super::ReticulumNode
//! [`spawn_interface`]: super::ReticulumNode::spawn_interface

use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::config::InterfaceConfig;
use crate::error::Error;
use crate::interfaces::InterfaceHandle;
use leviculum_core::transport::InterfaceId;

use super::AutoPeerCount;

mod auto;
#[cfg(any())]
mod ble;
mod i2p;
#[cfg(any())]
mod kiss;
#[cfg(any())]
mod pipe;
#[cfg(any())]
mod rnode;
#[cfg(any())]
mod rnode_multi;
#[cfg(any())]
mod serial;
mod tcp;
mod udp;

/// Shared wiring the per-type builders need, so they do not depend on the
/// driver struct itself.
pub(super) struct InterfaceBuildCtx<'a> {
    pub next_id: &'a Arc<AtomicUsize>,
    pub new_iface_tx: &'a mpsc::Sender<InterfaceHandle>,
    pub reconnect_tx: &'a mpsc::Sender<InterfaceId>,
    pub tunnel_notify_tx: &'a mpsc::Sender<InterfaceId>,
    /// Per-peer transition reports (loss + arrival) from multi-peer
    /// interfaces (Codeberg #365); today only the BLE builder hands it
    /// to its interface task — and that builder is Linux-only (it needs
    /// BlueZ over D-Bus), so off Linux this field has no reader and
    /// `-D warnings` would fail the lane.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub peer_event_tx: &'a mpsc::Sender<(InterfaceId, crate::interfaces::PeerEvent)>,
    /// Test-only frame-corruption cadence.
    pub corrupt_every: Option<u64>,
    /// Storage root; I2P persists its per-interface keyfile under it.
    pub storage_path: Option<PathBuf>,
    /// Applied to each TCP client's connect socket before it dials.
    pub outbound_socket_hook: Option<crate::socket_hook::OutboundSocketHook>,
    /// Reporting-side interface inventory (Codeberg #177): listeners register
    /// themselves here because they never become routable interfaces.
    pub inventory: crate::interfaces::inventory::SharedInventory,
    /// Whether transport is enabled, which decides the announce-rate defaults
    /// a listener reports (Reticulum.py:830-833).
    pub transport_enabled: bool,
    /// The node's 16-byte identity hash. The BLE interface publishes it in
    /// the Columba Identity characteristic, handshakes with it, and derives
    /// its advertised `LN-<hex8>` name from it — the same derivation the
    /// firmware uses (`leviculum_ble_tx::device_name`).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub identity_hash: [u8; 16],
}

/// Outcome of building one configured interface section.
pub(super) enum Built {
    /// Handles the caller registers itself (config init) or dispatches through
    /// `new_iface_tx` (runtime attach).
    Handles(Vec<InterfaceHandle>),
    /// A listener/orchestrator that registered its own children through
    /// `new_iface_tx`, or an unknown type; the caller registers nothing.
    SelfManaged,
}

/// Construct the interface(s) for one section.
///
/// `idx` is the base [`InterfaceId`]: leaf interfaces take it directly (stable
/// across restarts at startup), fan-out children draw further ids from
/// `ctx.next_id`.
pub(super) fn build_interface(
    idx: usize,
    config: &InterfaceConfig,
    ctx: &InterfaceBuildCtx<'_>,
    auto_peer_count: &AutoPeerCount,
) -> Result<Built, Error> {
    // TEST-ONLY `test_drop_direct_ingress` (range emulation for co-located
    // rigs) reads the wire hops byte at `raw[1]`, which only exists on an
    // unwrapped Reticulum frame: IFAC prepends authentication material
    // before the flags byte, so the two cannot coexist. And only the
    // LoRa-capable single-port types implement the filter. Both refused
    // here, centrally, so a misconfiguration fails daemon startup instead
    // of silently running without range emulation.
    if config.test_drop_direct_ingress {
        if !matches!(
            config.interface_type.as_str(),
            "RNodeInterface" | "SerialInterface"
        ) {
            return Err(Error::Config(format!(
                "interface '{}': test_drop_direct_ingress is only supported on \
                 RNodeInterface and SerialInterface, not {}",
                config.name, config.interface_type
            )));
        }
        if config.networkname.is_some() || config.passphrase.is_some() {
            return Err(Error::Config(format!(
                "interface '{}': test_drop_direct_ingress is incompatible with IFAC \
                 (networkname/passphrase): IFAC prepends material before the flags \
                 byte, so the wire hops byte is no longer at raw[1]; remove the IFAC \
                 keys or the test knob",
                config.name
            )));
        }
    }
    match config.interface_type.as_str() {
        "TCPClientInterface" => tcp::build_client(idx, config, ctx),
        "TCPServerInterface" => tcp::build_server(idx, config, ctx),
        "UDPInterface" => udp::build(idx, config, ctx),
        "AutoInterface" => auto::build(config, ctx, auto_peer_count),
        #[cfg(any())]
        "RNodeInterface" => rnode::build(idx, config, ctx),
        #[cfg(not(any()))]
        "RNodeInterface" => Err(Error::Config("RNodeInterface is disabled".to_string())),

        #[cfg(any())]
        "RNodeMultiInterface" => rnode_multi::build(idx, config, ctx),
        #[cfg(not(any()))]
        "RNodeMultiInterface" => Err(Error::Config("RNodeMultiInterface is disabled".to_string())),
        #[cfg(any())]
        "SerialInterface" => serial::build(idx, config, ctx),
        #[cfg(not(any()))]
        "SerialInterface" => Err(Error::Config("SerialInterface is disabled".to_string())),

        #[cfg(any())]
        "PipeInterface" => pipe::build(idx, config, ctx),
        #[cfg(not(any()))]
        "PipeInterface" => Err(Error::Config("PipeInterface is disabled".to_string())),

        #[cfg(any())]
        "KISSInterface" | "AX25KISSInterface" => kiss::build(idx, config, ctx),
        #[cfg(not(any()))]
        "KISSInterface" | "AX25KISSInterface" => Err(Error::Config("KISSInterface is disabled".to_string())),

        "I2PInterface" => i2p::build(idx, config, ctx),
        #[cfg(any())]
        "BLEInterface" => ble::build(idx, config, ctx),
        // A BLE section in a config file on a non-Linux host is a
        // configuration error worth naming, not a silent no-op.
        #[cfg(not(any()))]
        "BLEInterface" => Err(crate::Error::Config(
            "BLEInterface is supported on Linux only (it requires BlueZ over D-Bus)".to_string(),
        )),
        other => {
            tracing::warn!("Unknown interface type: {}", other);
            Ok(Built::SelfManaged)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Owned wiring a test `InterfaceBuildCtx` borrows from.
    struct CtxOwner {
        next_id: Arc<AtomicUsize>,
        new_iface_tx: mpsc::Sender<InterfaceHandle>,
        reconnect_tx: mpsc::Sender<InterfaceId>,
        tunnel_notify_tx: mpsc::Sender<InterfaceId>,
        peer_event_tx: mpsc::Sender<(InterfaceId, crate::interfaces::PeerEvent)>,
        inventory: crate::interfaces::inventory::SharedInventory,
    }

    impl CtxOwner {
        fn new() -> Self {
            Self::with_iface_channel(4).0
        }

        /// Like [`new`](Self::new) but with a caller-sized new-interface
        /// channel, returning its receiver so a test can drain or fill it.
        fn with_iface_channel(capacity: usize) -> (Self, mpsc::Receiver<InterfaceHandle>) {
            let (new_iface_tx, new_iface_rx) = mpsc::channel(capacity);
            let (reconnect_tx, _reconnect_rx) = mpsc::channel(4);
            let (tunnel_notify_tx, _tunnel_notify_rx) = mpsc::channel(4);
            let (peer_event_tx, _peer_event_rx) = mpsc::channel(4);
            (
                Self {
                    next_id: Arc::new(AtomicUsize::new(100)),
                    new_iface_tx,
                    reconnect_tx,
                    tunnel_notify_tx,
                    peer_event_tx,
                    inventory: crate::interfaces::inventory::InterfaceInventory::shared(),
                },
                new_iface_rx,
            )
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
                identity_hash: [0u8; 16],
            }
        }
    }

    fn rnode_config(frequency: u64) -> InterfaceConfig {
        InterfaceConfig {
            interface_type: "RNodeInterface".to_string(),
            port: Some("/dev/nonexistent-test-port".to_string()),
            frequency: Some(frequency),
            bandwidth: Some(125_000),
            spreading_factor: Some(7),
            coding_rate: Some(5),
            ..Default::default()
        }
    }

    /// A 125 kHz carrier centred at 868.65 MHz sits in the 868.6-868.7 MHz
    /// alarm band (<= 25 kHz channel spacing only). It builds — the operator
    /// carries the regulatory responsibility, and the same carrier is lawful
    /// under a licence or in a shielded chamber — but it does not build
    /// quietly: the band is named at WARN, where no debug filter can hide it.
    ///
    /// Until 2026-08-27 this was an `Err`, which is the one refusal on the
    /// RNode path that was about radio law rather than device capability.
    #[tokio::test]
    async fn rnode_build_warns_about_a_carrier_in_an_alarm_band_and_honours_it() {
        let owner = CtxOwner::new();
        let (seen, subscriber) = warn_tap();
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            rnode::build(0, &rnode_config(868_650_000), &owner.ctx())
                .expect("868.65 MHz / 125 kHz builds; it is warned about, not refused");
        }
        let warnings = seen.lock().expect("warn tap");
        assert!(
            warnings.iter().any(|w| w.contains("868.6-868.7 MHz")),
            "the band is named at WARN: {warnings:?}"
        );
    }

    /// The same block one sub-band over builds fine — the refusal is the gap,
    /// not the neighbourhood. Needs a runtime because a successful build
    /// spawns the interface tasks (the port itself may fail later; the
    /// reconnect loop owns that).
    #[tokio::test]
    async fn rnode_build_accepts_a_carrier_in_a_listed_sub_band() {
        let owner = CtxOwner::new();
        assert!(rnode::build(0, &rnode_config(869_525_000), &owner.ctx()).is_ok());
    }

    /// The `SerialInterface` LNode path treats the same carrier the same way:
    /// it builds, and the band is named at WARN before `serial_radio_config`
    /// resolves anything.
    ///
    /// Until 2026-08-27 this was an `Err`. The 2026-08-27 audit enumerated the
    /// refusals reachable from an *RNode* build and missed this one, because a
    /// `SerialInterface` with a `frequency` is the LNode LoRa modem, not an
    /// RNode — the same predicate, the same band, the same reason, on the path
    /// our own hardware uses. Needs a runtime: a successful build spawns the
    /// interface tasks.
    #[tokio::test]
    async fn serial_build_warns_about_a_carrier_in_an_alarm_band_and_honours_it() {
        let owner = CtxOwner::new();
        let config = InterfaceConfig {
            interface_type: "SerialInterface".to_string(),
            port: Some("/dev/nonexistent-test-port".to_string()),
            frequency: Some(868_650_000),
            ..Default::default()
        };
        let (seen, subscriber) = warn_tap();
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            serial::build(0, &config, &owner.ctx())
                .expect("868.65 MHz / 125 kHz builds; it is warned about, not refused");
        }
        let warnings = seen.lock().expect("warn tap");
        assert!(
            warnings.iter().any(|w| w.contains("868.6-868.7 MHz")),
            "the band is named at WARN: {warnings:?}"
        );
    }

    /// Every `warn!` message emitted while the returned guard lives.
    fn warn_tap() -> (
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        impl tracing::Subscriber + Send + Sync,
    ) {
        use tracing::field::{Field, Visit};
        use tracing_subscriber::layer::SubscriberExt;

        struct Sink(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        impl Visit for Sink {
            fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
                if f.name() == "message" {
                    if let Ok(mut seen) = self.0.lock() {
                        seen.push(format!("{v:?}"));
                    }
                }
            }
        }

        struct Layer(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Layer {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                if *event.metadata().level() == tracing::Level::WARN {
                    event.record(&mut Sink(std::sync::Arc::clone(&self.0)));
                }
            }
        }

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            std::sync::Arc::clone(&seen),
            tracing_subscriber::registry().with(Layer(seen)),
        )
    }

    /// The #315 ceiling warning reaches the daemon log, and only for the block
    /// that crosses it. The pure decision is tested in `interfaces::serial`;
    /// what this adds is the wiring — a warning nothing calls encodes nothing.
    ///
    /// Both halves are the control for each other: same builder, same PHY,
    /// same port, one differing key.
    #[tokio::test]
    async fn a_preamble_pin_over_the_ceiling_is_warned_at_build_time() {
        let lora_block = |preamble: u16| InterfaceConfig {
            interface_type: "SerialInterface".to_string(),
            port: Some("/dev/nonexistent-test-port".to_string()),
            frequency: Some(869_525_000),
            bandwidth: Some(125_000),
            spreading_factor: Some(10),
            coding_rate: Some(8),
            preamble_symbols: Some(preamble),
            ..Default::default()
        };

        let owner = CtxOwner::new();
        let (seen, subscriber) = warn_tap();
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            serial::build(0, &lora_block(24), &owner.ctx()).expect("24 builds, it only warns");
            serial::build(1, &lora_block(18), &owner.ctx()).expect("18 builds silently");
        }

        let warnings = seen.lock().expect("warn tap");
        assert_eq!(
            warnings.len(),
            1,
            "exactly the pinned-24 block warns: {warnings:?}"
        );
        assert!(
            warnings[0].contains("#315") && warnings[0].contains("serial_0"),
            "the warning names the issue and the interface: {}",
            warnings[0]
        );
    }

    /// TEST-ONLY `test_drop_direct_ingress` is refused together with IFAC:
    /// IFAC prepends material before the flags byte, so the wire hops byte
    /// the filter reads is no longer at `raw[1]`. The refusal is a config
    /// error at build time, on both LoRa-capable types.
    #[test]
    fn drop_direct_ingress_with_ifac_is_a_config_error() {
        let owner = CtxOwner::new();
        for (iface_type, extra_port_keys) in [("RNodeInterface", true), ("SerialInterface", false)]
        {
            let mut config = InterfaceConfig {
                name: "deaf".to_string(),
                interface_type: iface_type.to_string(),
                port: Some("/dev/nonexistent-test-port".to_string()),
                frequency: Some(869_525_000),
                test_drop_direct_ingress: true,
                networkname: Some("testnet".to_string()),
                ..Default::default()
            };
            if extra_port_keys {
                config.bandwidth = Some(125_000);
                config.spreading_factor = Some(7);
                config.coding_rate = Some(5);
            }
            let err = build_interface(0, &config, &owner.ctx(), &AutoPeerCount::default())
                .err()
                .unwrap_or_else(|| panic!("{iface_type}: IFAC + knob must not build"));
            let msg = err.to_string();
            assert!(
                msg.contains("incompatible with IFAC"),
                "{iface_type}: {msg}"
            );
        }
    }

    /// The knob on a type that does not implement the filter is refused
    /// rather than silently ignored — a rig config that thinks it emulates
    /// range but does not would invalidate a hardware run.
    #[test]
    fn drop_direct_ingress_on_an_unsupported_type_is_a_config_error() {
        let owner = CtxOwner::new();
        let config = InterfaceConfig {
            name: "deaf-tcp".to_string(),
            interface_type: "TCPClientInterface".to_string(),
            target_host: Some("127.0.0.1".to_string()),
            target_port: Some(4242),
            test_drop_direct_ingress: true,
            ..Default::default()
        };
        let err = build_interface(0, &config, &owner.ctx(), &AutoPeerCount::default())
            .err()
            .expect("knob on TCP must not build");
        let msg = err.to_string();
        assert!(msg.contains("only supported on"), "{msg}");
    }

    /// The multi builder checks each subinterface's own frequency, and warns
    /// per subinterface rather than refusing the whole port: one
    /// out-of-sub-band vport must not take the other three off the air.
    #[tokio::test]
    async fn rnode_multi_build_warns_about_a_subinterface_in_an_alarm_band() {
        let owner = CtxOwner::new();
        let config = InterfaceConfig {
            interface_type: "RNodeMultiInterface".to_string(),
            port: Some("/dev/nonexistent-test-port".to_string()),
            subinterfaces: vec![crate::config::SubinterfaceConfig {
                name: "gap".to_string(),
                vport: Some(0),
                frequency: Some(868_650_000),
                bandwidth: Some(125_000),
                spreading_factor: Some(7),
                coding_rate: Some(5),
                ..Default::default()
            }],
            ..Default::default()
        };
        let (seen, subscriber) = warn_tap();
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            rnode_multi::build(0, &config, &owner.ctx())
                .expect("868.65 MHz / 125 kHz builds; it is warned about, not refused");
        }
        let warnings = seen.lock().expect("warn tap");
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("868.6-868.7 MHz") && w.contains("'gap'")),
            "the band and the subinterface are named at WARN: {warnings:?}"
        );
    }

    // ------------------------------------------------------------------
    // The regulatory guard
    // ------------------------------------------------------------------
    //
    // Project policy, decided 2026-08-16 and restated 2026-08-27:
    //
    //   No radio configuration is ever refused for a radio-regulatory
    //   reason. It is warned about, loudly, and then honoured.
    //
    // The reasoning is not only that we cannot know the operator's
    // jurisdiction. In the EU the operator, not the manufacturer, carries the
    // legal responsibility for compliant operation, so software that refuses
    // a setting takes on a responsibility it does not hold — and blocks
    // operation that is lawful elsewhere, under a licence, or in a shielded
    // chamber.
    //
    // The rule was written down and then drifted for eleven days without
    // anyone noticing. Prose has failed twice, so this is the mechanical
    // form. It is behavioural rather than source-level (an assertion that no
    // `Err` arm in this module is reachable from a regulatory predicate) for
    // three reasons:
    //
    //   * a source-level check cannot tell a regulatory predicate from a
    //     capability one without a hand-maintained list of predicate names,
    //     and that list is exactly the prose that already drifted;
    //   * it proves nothing about what a config file does — a refusal moved
    //     one call deeper, into `validate_config` or a new helper, passes it
    //     while the operator's daemon still will not start;
    //   * it goes stale the first time the module is refactored, whereas
    //     these cases are stated in the operator's own terms.
    //
    // The cost is a spawned interface per case against a port that does not
    // exist, which the reconnect loop already tolerates.
    //
    // The two halves are one guard. `no_radio_configuration_is_refused_for_a_
    // regulatory_reason` alone would be satisfied by deleting every check in
    // the file, so `capability_refusals_are_untouched_by_the_regulatory_guard`
    // pins the five refusals that must stay: the chip's tuning range, the ten
    // LoRa bandwidths, the RNode wire field's 0..=37, and the SF/CR ranges
    // shared with Python-RNS. Those are arithmetic, not paternalism.

    /// One radio block, so each case differs from the lawful control by
    /// exactly the key under test.
    #[derive(Clone, Copy)]
    struct Radio {
        frequency: u64,
        bandwidth: u32,
        sf: u8,
        cr: u8,
        tx_power: Option<i8>,
        airtime_limit_long: Option<f64>,
    }

    impl Radio {
        /// 869.525 MHz / BW125 / SF7 / CR4:5 — ERC 70-03 sub-band h1.7,
        /// inside every listed limit, with nothing explicitly configured that
        /// a regulatory predicate could object to.
        fn lawful() -> Self {
            Self {
                frequency: 869_525_000,
                bandwidth: 125_000,
                sf: 7,
                cr: 5,
                tx_power: None,
                airtime_limit_long: None,
            }
        }

        fn single(&self) -> InterfaceConfig {
            InterfaceConfig {
                name: "guard".to_string(),
                interface_type: "RNodeInterface".to_string(),
                port: Some("/dev/nonexistent-test-port".to_string()),
                frequency: Some(self.frequency),
                bandwidth: Some(self.bandwidth),
                spreading_factor: Some(self.sf),
                coding_rate: Some(self.cr),
                tx_power: self.tx_power,
                airtime_limit_long: self.airtime_limit_long,
                ..Default::default()
            }
        }

        fn multi(&self) -> InterfaceConfig {
            InterfaceConfig {
                name: "guard".to_string(),
                interface_type: "RNodeMultiInterface".to_string(),
                port: Some("/dev/nonexistent-test-port".to_string()),
                subinterfaces: vec![crate::config::SubinterfaceConfig {
                    name: "guard".to_string(),
                    vport: Some(0),
                    frequency: Some(self.frequency),
                    bandwidth: Some(self.bandwidth),
                    spreading_factor: Some(self.sf),
                    coding_rate: Some(self.cr),
                    tx_power: self.tx_power,
                    airtime_limit_long: self.airtime_limit_long,
                    ..Default::default()
                }],
                ..Default::default()
            }
        }

        /// The LNode LoRa modem: a `SerialInterface` block with a `frequency`.
        /// Not an RNode — it speaks our own firmware, not the RNode KISS
        /// dialect — but it carries the same radio keys, so the same policy
        /// binds it. This is the shape the 2026-08-27 audit missed.
        fn serial(&self) -> InterfaceConfig {
            InterfaceConfig {
                name: "guard".to_string(),
                interface_type: "SerialInterface".to_string(),
                port: Some("/dev/nonexistent-test-port".to_string()),
                frequency: Some(self.frequency),
                bandwidth: Some(self.bandwidth),
                spreading_factor: Some(self.sf),
                coding_rate: Some(self.cr),
                tx_power: self.tx_power,
                airtime_limit_long: self.airtime_limit_long,
                ..Default::default()
            }
        }
    }

    /// Every shape a radio configuration can arrive in, so a refusal cannot
    /// survive by living only in one the guard does not drive. Driven through
    /// `build_interface`, the dispatch a config file actually reaches, not the
    /// submodule entry points.
    fn radio_shapes(radio: &Radio) -> [(&'static str, InterfaceConfig); 3] {
        [
            ("RNodeInterface", radio.single()),
            ("RNodeMultiInterface", radio.multi()),
            ("SerialInterface", radio.serial()),
        ]
    }

    /// The shapes that validate device capability at build time — the RNode
    /// family, both of them, via `leviculum_core::rnode::validate_config`.
    /// `SerialInterface` is absent because it validates nothing: it resolves
    /// its radio block and hands it to the modem. See
    /// `capability_refusals_are_untouched_by_the_regulatory_guard`.
    fn rnode_shapes(radio: &Radio) -> [(&'static str, InterfaceConfig); 2] {
        [
            ("RNodeInterface", radio.single()),
            ("RNodeMultiInterface", radio.multi()),
        ]
    }

    /// One regulatory edge case: the block, the substring the warning it must
    /// provoke carries, and the shapes that can carry it.
    ///
    /// Every shape must *build* every case — that half is unconditional, it is
    /// the policy itself. `warns_on` is narrower because the warning comes out
    /// wherever the key is resolved, and `SerialInterface` resolves
    /// `airtime_limit_long` through the firmware's own silent derivation
    /// (`leviculum_core::rnode::firmware_default_lt_alock`) rather than the
    /// driver's `resolve_lt_alock`, which is where that warning lives. The gap
    /// is data here, not prose, so it cannot widen unnoticed: a shape added to
    /// `radio_shapes` is refusal-checked whether or not anyone remembers it.
    struct Case {
        what: &'static str,
        radio: Radio,
        expected: &'static str,
        warns_on: &'static [&'static str],
    }

    /// The guard. Every known regulatory edge case builds, and says so out
    /// loud. See the block comment above for why this shape.
    ///
    /// "Warns" is asserted at WARN specifically: a decision narrated at debug
    /// is invisible in a default daemon log, which is the silent-substitution
    /// defect of Codeberg #349/#350 rather than a fix for it.
    #[tokio::test]
    async fn no_radio_configuration_is_refused_for_a_regulatory_reason() {
        const RADIO_SHAPES: &[&str] = &["RNodeInterface", "RNodeMultiInterface", "SerialInterface"];
        let cases: [Case; 3] = [
            Case {
                what: "a carrier occupying the 868.6-868.7 MHz narrowband alarm band",
                radio: Radio {
                    frequency: 868_650_000,
                    ..Radio::lawful()
                },
                expected: "868.6-868.7 MHz",
                warns_on: RADIO_SHAPES,
            },
            Case {
                what: "22 dBm on 867.2 MHz, 8 dB over the 14 dBm h1.4 e.r.p. limit",
                radio: Radio {
                    frequency: 867_200_000,
                    tx_power: Some(22),
                    ..Radio::lawful()
                },
                expected: "exceeds the derived ERP limit",
                warns_on: RADIO_SHAPES,
            },
            Case {
                what: "the ETSI duty cycle switched off outright",
                radio: Radio {
                    airtime_limit_long: Some(0.0),
                    ..Radio::lawful()
                },
                expected: "exceeds the ETSI EU868 lawful default",
                // Not SerialInterface: see `Case`.
                warns_on: &["RNodeInterface", "RNodeMultiInterface"],
            },
        ];

        let owner = CtxOwner::new();
        for case in cases {
            for (idx, (shape, config)) in radio_shapes(&case.radio).into_iter().enumerate() {
                let (seen, subscriber) = warn_tap();
                let built = {
                    let _guard = tracing::subscriber::set_default(subscriber);
                    build_interface(idx, &config, &owner.ctx(), &AutoPeerCount::default())
                };
                if let Err(e) = built {
                    panic!(
                        "{shape}: {} was refused, which project policy forbids \
                         for a regulatory reason: {e}",
                        case.what
                    );
                }
                if !case.warns_on.contains(&shape) {
                    continue;
                }
                let warnings = seen.lock().expect("warn tap");
                assert!(
                    warnings.iter().any(|w| w.contains(case.expected)),
                    "{shape}: {} built silently; a warning containing {:?} \
                     must reach the daemon log at WARN. Seen: {warnings:?}",
                    case.what,
                    case.expected
                );
            }
        }
    }

    /// The other half. A refusal about what the chip or the wire format can
    /// carry is arithmetic, not radio law, and must survive the guard above
    /// intact — a guard that also forbade these would be worse than none.
    ///
    /// Driven over `rnode_shapes`, not `radio_shapes`: `SerialInterface` has
    /// no capability validation to pin. It resolves its radio block and hands
    /// it to the LNode firmware without calling `validate_config`, so all five
    /// cases below build there. That is a gap in *this* half — an unbuildable
    /// PHY reaches the modem instead of failing daemon startup — and it is a
    /// separate concern from the regulatory policy, so it is recorded rather
    /// than fixed here. The lawful control below does cover all three shapes.
    #[tokio::test]
    async fn capability_refusals_are_untouched_by_the_regulatory_guard() {
        let cases: [(&str, Radio); 5] = [
            (
                "below the transceiver's 137 MHz tuning floor",
                Radio {
                    frequency: 100_000_000,
                    ..Radio::lawful()
                },
            ),
            (
                "not one of the ten LoRa bandwidths",
                Radio {
                    bandwidth: 100_000,
                    ..Radio::lawful()
                },
            ),
            (
                "over the RNode wire field's 0..=37 dBm",
                Radio {
                    tx_power: Some(38),
                    ..Radio::lawful()
                },
            ),
            (
                "outside the LoRa spreading factors 5..=12",
                Radio {
                    sf: 13,
                    ..Radio::lawful()
                },
            ),
            (
                "outside the LoRa coding rates 5..=8",
                Radio {
                    cr: 9,
                    ..Radio::lawful()
                },
            ),
        ];

        let owner = CtxOwner::new();
        // Control: the block every case is one key away from must build, or
        // the assertions below would pass for the wrong reason. All three
        // shapes, because it is also the control for the regulatory guard.
        for (shape, config) in radio_shapes(&Radio::lawful()) {
            build_interface(0, &config, &owner.ctx(), &AutoPeerCount::default())
                .unwrap_or_else(|e| panic!("{shape}: the lawful control must build: {e}"));
        }

        for (case, radio) in cases {
            for (idx, (shape, config)) in rnode_shapes(&radio).into_iter().enumerate() {
                assert!(
                    build_interface(idx, &config, &owner.ctx(), &AutoPeerCount::default()).is_err(),
                    "{shape}: {case} is a capability limit, not radio law, and must \
                     still be refused"
                );
            }
        }
    }

    /// L-0063: an I2P peer sub-interface that cannot be registered (the
    /// new-interface channel is full) must not be spawned at all. The old
    /// path spawned the client task, then dropped the handle on `try_send`
    /// failure — the task kept building SAM tunnels the driver could
    /// neither see nor tear down. The id counter is the spawn's footprint:
    /// each spawned peer allocates exactly one id right before its spawn,
    /// so a full channel must leave the counter untouched for that peer.
    #[tokio::test]
    async fn i2p_peer_that_cannot_register_is_not_spawned() {
        let (owner, mut new_iface_rx) = CtxOwner::with_iface_channel(1);
        let config = InterfaceConfig {
            interface_type: "I2PInterface".to_string(),
            peers: Some(vec![
                "peer-a.b32.i2p".to_string(),
                "peer-b.b32.i2p".to_string(),
            ]),
            ..Default::default()
        };

        let before = owner.next_id.load(std::sync::atomic::Ordering::Relaxed);
        i2p::build(0, &config, &owner.ctx()).expect("i2p peers build");

        // Exactly one peer fits the channel...
        assert!(
            new_iface_rx.try_recv().is_ok(),
            "the first peer must register"
        );
        assert!(
            new_iface_rx.try_recv().is_err(),
            "the second peer cannot fit a capacity-1 channel"
        );
        // ...and only that one may have been spawned.
        let allocated = owner.next_id.load(std::sync::atomic::Ordering::Relaxed) - before;
        assert_eq!(
            allocated, 1,
            "L-0063: a peer that cannot register was spawned anyway \
             ({allocated} ids allocated for 1 registrable slot)"
        );
    }
}
