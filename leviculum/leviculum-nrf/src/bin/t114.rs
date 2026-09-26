//! Firmware entry point for Heltec Mesh Node T114
//!
//! Runs a Reticulum transport node with three interfaces:
//! - Interface 0: USB CDC-ACM serial (HDLC framing) to host
//! - Interface 1: SX1262 LoRa radio
//! - Interface 2: BLE peripheral (Columba v2.2 protocol)
//!
//! The transport engine routes packets between all interfaces.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;
use embassy_executor::Spawner;
use embassy_futures::select::{select, select4, Either, Either4};
use embassy_nrf::gpio::Level;
use embassy_nrf::spim;
use embassy_time::{Duration, Instant, Timer};

use leviculum_core::embedded_storage::EmbeddedStorage;
use leviculum_core::ifac::IfacConfig;
use leviculum_core::node::{NodeCoreBuilder, NodeEvent};
use leviculum_core::traits::Interface;
use leviculum_core::transport::dispatch_actions;
use leviculum_core::InterfaceId;

use leviculum_nrf::ble::{BleInterface, PeerEvent as BlePeerEvent};
use leviculum_nrf::boards::t114;
use leviculum_nrf::clock::EmbassyClock;
use leviculum_nrf::interface::EmbeddedInterface;
use leviculum_nrf::lora::LoRaInterface;
use leviculum_nrf::{info, init_heap, log_critical};

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // First statement on purpose: reads the PREVIOUS boot's breadcrumbs
    // and RESETREAS (and clears it), then arms this boot's record at
    // enter-main — so even a hang inside `embassy_nrf::init`'s HFXO wait
    // is attributable on the next boot's BOOT_TRACE line.
    let boot = leviculum_nrf::boot_trace::capture();

    let mut config = embassy_nrf::config::Config::default();
    config.hfclk_source = embassy_nrf::config::HfclkSource::ExternalXtal;
    config.gpiote_interrupt_priority = embassy_nrf::interrupt::Priority::P2;
    config.time_interrupt_priority = embassy_nrf::interrupt::Priority::P2;
    let p = embassy_nrf::init(config);

    init_heap();
    leviculum_nrf::init_tracing();

    // Boot diagnostics from the previous run. Read before the first log
    // call of this boot: `take_persistent_log` must snapshot the PREVIOUS
    // boot's tail before we start writing this boot's into the same ring.
    let hardfault_pm = leviculum_nrf::take_hardfault_postmortem();
    let panic_pm = leviculum_nrf::take_panic_postmortem();
    let persistent_log = leviculum_nrf::log::take_persistent_log();

    // SAFETY: called once before any complex work or concurrent tasks.
    // Under flip-link the painted region is `[_stack_end, SP-1KiB)` at
    // the BOTTOM of RAM — disjoint from `.uninit`, so the post-mortem
    // reads above are no longer order-critical against it.
    unsafe {
        leviculum_nrf::paint_stack();
    }

    leviculum_nrf::set_panic_led(
        t114::PANIC_LED_PORT,
        t114::PANIC_LED_PIN,
        t114::PANIC_LED_ACTIVE_LOW,
    );
    leviculum_nrf::set_irq_priorities();
    // This binary wires a telemetry Reporter into its main loop; declared
    // before USB comes up so the serial task can never answer a telemetry
    // target ahead of the declaration (ack honesty, #236).
    leviculum_nrf::telemetry::declare_reporter();
    // Which position sources this board boots with — the second clause of
    // the telemetry send condition, and a question a host may put before
    // the reporter exists, so it is answered before USB comes up. The
    // GNSS bit says a receiver is *configured*, not that it has a fix:
    // reports run `position=0` until the L76K acquires and `position=1
    // possrc=gnss` after (#69, #255). `cfg!` and not a constant, because
    // the spawn below is under the same gate — the two must not be able
    // to disagree.
    leviculum_nrf::telemetry::declare_position_sources(
        /* gnss_available */ cfg!(feature = "gnss"),
        t114::CONFIG.telemetry_flash_page,
    );
    // The media profile decides which carriers come up at all, so it is
    // read before USB and before either carrier: before USB so a host
    // frame can never be answered against the default while the flash
    // record is still unread, and before the carriers because the answer
    // is their spawn decision. A memory-mapped flash read, legal this
    // early and before `Softdevice::enable`.
    let (media, media_src) = leviculum_nrf::media::load_at_boot(t114::CONFIG.telemetry_flash_page);
    // The node's name, read on the same page and for the same reason the
    // profile is: before USB, so a host frame is never answered against
    // the derived default while the record is still unread, and before
    // the first announce and `ble::init`, which both display it.
    leviculum_nrf::name::load_at_boot(t114::CONFIG.telemetry_flash_page);
    leviculum_nrf::boot_trace::phase(leviculum_nrf::boot_trace::Phase::PersistRead);
    let vbus = leviculum_nrf::init_vbus();
    let serial = leviculum_nrf::usb::init(&spawner, p.USBD, vbus, &t114::CONFIG);
    leviculum_nrf::boot_trace::phase(leviculum_nrf::boot_trace::Phase::UsbUp);

    log_critical!("leviculum T114 booting");
    log_critical!("[FW_BUILD] {}", leviculum_nrf::FW_BUILD_STAMP);
    log_critical!("[TIME_SOURCE] source={}", leviculum_nrf::time_source_str());
    leviculum_nrf::log_stack("boot");
    leviculum_nrf::log_panic_count();
    // Boot-loop instrumentation: how far did the previous boot get, and
    // what kind of reset got us here? Both values were captured by
    // `boot_trace::capture` at the top of main (POWER was still ours
    // there; after Softdevice::enable it belongs to the SD).
    leviculum_nrf::boot_trace::log_prev(&boot);
    leviculum_nrf::log_reset_reason(boot.reset_reason);
    leviculum_nrf::log_irq_priorities();

    // Shared boot/query formatter — the same block is retrievable at any
    // later time via the debug-port query (`p` byte, postmortem_query).
    leviculum_nrf::log_postmortems(hardfault_pm.as_ref(), panic_pm.as_ref());
    if let Some(snap) = persistent_log {
        let mut start = 0usize;
        while start < snap.len {
            let end = snap.bytes[start..snap.len]
                .iter()
                .position(|&b| b == b'\n')
                .map(|p| start + p + 1)
                .unwrap_or(snap.len);
            if start == 0 && end == snap.len {
                // single-line case
            } else if start == 0 {
                start = end;
                continue;
            }
            let raw = &snap.bytes[start..end];
            let trimmed = core::str::from_utf8(raw)
                .unwrap_or("<non-utf8>")
                .trim_end_matches(['\r', '\n']);
            if !trimmed.is_empty() {
                log_critical!("[PERSISTENT_LOG] {}", trimmed);
            }
            start = end;
        }
    }

    // VEXT (P0.21) carries BOTH the TFT chain and the L76K receiver, so
    // it belongs to neither task; the binary raises it here, as the
    // references do at board level (Meshtastic main.cpp:420-422 "turn on
    // the display power"; RNode setup()). Raised as early as the
    // peripherals exist so its 1 s warmup overlaps the rest of init
    // instead of delaying a consumer.
    leviculum_nrf::vext::raise(p.P0_21);
    let mut led = t114::led(p.P1_03);

    let rng = leviculum_nrf::rng::RawHwRng::new();

    // Load or generate persistent identity from internal flash
    let mut id_store = leviculum_nrf::flash::NvmcIdentityStore::new(
        embassy_nrf::nvmc::Nvmc::new(p.NVMC),
        t114::CONFIG.identity_flash_page,
    );

    let mut builder = NodeCoreBuilder::new()
        .enable_transport(true)
        .max_incoming_resource_size(8 * 1024)
        .max_queued_announces(32)
        .max_random_blobs(8)
        .respond_to_probes(true);

    let identity_loaded = {
        use leviculum_core::identity_store::IdentityStore;
        if let Ok(Some(identity)) = id_store.load() {
            info!("Identity loaded from flash");
            builder = builder.identity(identity);
            true
        } else {
            info!("No identity in flash, generating new");
            false
        }
    };

    let mut node = builder.build_boxed(rng, EmbassyClock, EmbeddedStorage::new());

    let initial_path_len = node.path_count();
    info!("[BOOT] path_table_initial_len={}", initial_path_len);
    spawner.must_spawn(boot_log_repeater(initial_path_len));
    spawner.must_spawn(fw_build_banner(media_src));
    spawner.must_spawn(leviculum_nrf::heap_watermark_task());
    spawner.must_spawn(leviculum_nrf::stack_watermark_task());

    if !identity_loaded {
        use leviculum_core::identity_store::IdentityStore;
        let _ = id_store.save(node.identity());
        info!("Identity saved to flash");
    }

    // Register all three interfaces
    node.set_interface_name(0, alloc::string::String::from("serial_usb"));
    node.set_interface_hw_mtu(0, 564);
    // Codeberg #117: the client-facing serial interface must be Gateway so the
    // node discovers unknown paths on behalf of the host.
    node.set_interface_mode(0, leviculum_core::InterfaceMode::Gateway);
    node.set_interface_name(1, alloc::string::String::from("lora_sx1262"));
    node.set_interface_hw_mtu(1, 255);
    node.set_interface_name(2, alloc::string::String::from("ble"));
    node.set_interface_hw_mtu(2, 564);

    let hash = node.identity().hash();
    info!(
        "LNode started -- identity: {:02X}{:02X}{:02X}{:02X}{:02X}",
        hash[0], hash[1], hash[2], hash[3], hash[4]
    );
    // Full identity hash for benchmark trace correlation
    leviculum_nrf::log::log_fmt("[IDENTITY] ", format_args!(
        "t114_node={:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        hash[0], hash[1], hash[2], hash[3], hash[4], hash[5], hash[6], hash[7],
        hash[8], hash[9], hash[10], hash[11], hash[12], hash[13], hash[14], hash[15]
    ));
    if let Some(probe_hash) = node.probe_dest_hash() {
        let ph = probe_hash.as_bytes();
        leviculum_nrf::log::log_fmt("[IDENTITY] ", format_args!(
            "t114_probe={:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            ph[0], ph[1], ph[2], ph[3], ph[4], ph[5], ph[6], ph[7],
            ph[8], ph[9], ph[10], ph[11], ph[12], ph[13], ph[14], ph[15]
        ));
    }

    // QSPI NOR (MX25R1635F, 2 MB) — Codeberg #384 part 1: identify the
    // part, prove the bus, say what is on it. Nothing writes to it yet.
    // Before LoRa so a hang inside `Qspi::new` leaves the marker above as
    // the last line of the capture and of the persistent tail.
    log_critical!("[STG] qspi-init");
    if let Some(mut flash) = leviculum_nrf::qspi::identify_at_boot(
        p.QSPI,
        p.P1_14.into(), // SCK
        p.P1_15.into(), // CSN
        p.P1_12.into(), // IO0
        p.P1_13.into(), // IO1
        p.P0_07.into(), // IO2 / WP#
        p.P0_05.into(), // IO3 / HOLD#
        t114::CONFIG.qspi_part,
    ) {
        leviculum_nrf::qspi::log_head(&mut flash);
        leviculum_nrf::qspi::log_store(flash, t114::CONFIG.qspi_part);
    }

    // LoRa (SPIM2. SPIM3 has a MISO read bug on T114)
    log_critical!("[STG] lora-init");
    let lora = leviculum_nrf::lora::init(
        p.SPI2,
        p.P0_19.into(),
        p.P0_22.into(),
        p.P0_23.into(),
        p.P0_24.into(),
        p.P0_25.into(),
        p.P0_17.into(),
        p.P0_20.into(),
        spim::Frequency::M4,
        t114::CONFIG.lora_tcxo_voltage_reg,
    )
    .await;
    info!("SX1262 ready");

    // Radio profile: whatever a host last set and we persisted, else the
    // compiled default. A blank or corrupt page decodes to None.
    let radio_cfg = match leviculum_nrf::radio_store::load(t114::CONFIG.radio_config_flash_page)
        .and_then(leviculum_nrf::lora::RadioConfig::from_wire_config)
    {
        Some(cfg) => {
            leviculum_nrf::log::log_fmt(
                "[RADIO] ",
                format_args!(
                    "persisted freq={} bw={} sf={} cr={} pwr={}",
                    cfg.frequency_hz, cfg.bw_hz, cfg.sf, cfg.cr_denom, cfg.tx_power_dbm
                ),
            );
            cfg
        }
        None => {
            leviculum_nrf::log::log_fmt("[RADIO] ", format_args!("default eu_medium"));
            leviculum_nrf::lora::RadioConfig::eu_medium()
        }
    };
    let lora_channels = leviculum_nrf::lora::channels();
    // `lora=off` means the radio stays down: the task that resets,
    // configures and keys the SX1262 is never spawned, so the chip is
    // never brought out of the state `lora::init` left it in. Nothing
    // transmits and nothing is received — which is the point, since a
    // reception over the medium under test's neighbour is exactly what
    // makes a single-medium measurement falsifiable.
    if media.lora_enabled {
        // Per-board entropy for the TX channel-access randomness: a fixed
        // seed here would make co-booted boards draw identical pre-TX
        // jitter and collide anyway (see `lora_task`).
        let channel_seed = {
            use rand_core::RngCore as _;
            leviculum_nrf::rng::RawHwRng::new().next_u32()
        };
        spawner.must_spawn(leviculum_nrf::lora::lora_task(
            lora,
            radio_cfg,
            channel_seed,
        ));
        leviculum_nrf::boot_trace::phase(leviculum_nrf::boot_trace::Phase::LoraTask);
    } else {
        leviculum_nrf::media::log_carrier_held_down("lora");
        leviculum_nrf::boot_trace::phase(leviculum_nrf::boot_trace::Phase::LoraSkipped);
    }

    // BLE — full init restored. RAM ORIGIN bumped to 40K (memory.x) to give
    // Softdevice::enable headroom for our config (att_mtu=256, …).
    let identity_hash = *node.identity().hash();
    // Freeze the BLE name for this boot, before the SoftDevice is
    // enabled and before the advertisement is built: both read it, and a
    // name that changed between them would put two different strings on
    // the two BLE surfaces. This also publishes the identity hash the
    // control-envelope report derives its defaults from.
    leviculum_nrf::name::note_boot_name(&identity_hash);
    log_critical!("[STG] ble-init");
    let sd = leviculum_nrf::ble::init(
        &spawner,
        media.ble_enabled,
        identity_hash,
        vbus,
        p.RTC0,
        p.TIMER0,
        p.TEMP,
        p.PPI_CH19,
        p.PPI_CH30,
        p.PPI_CH31,
        p.PPI_CH17,
        p.PPI_CH18,
        p.PPI_CH20,
        p.PPI_CH21,
        p.PPI_CH22,
        p.PPI_CH23,
        p.PPI_CH24,
        p.PPI_CH25,
        p.PPI_CH26,
        p.PPI_CH27,
        p.PPI_CH28,
        p.PPI_CH29,
        p.RNG,
    );
    let ble_channels = leviculum_nrf::ble::channels();
    info!("BLE ready");

    // Both spawn decisions are made: say what actually came up, then
    // prove it on the debug port. `note_boot_state` is passed what was
    // really spawned, which is what makes "switching a medium on takes
    // effect at reboot" a fact the board can state rather than a hope.
    leviculum_nrf::media::note_boot_state(media.lora_enabled, media.ble_enabled);
    leviculum_nrf::media::log_banner(media_src);
    leviculum_nrf::name::log_banner();

    // Radio-config persistence. Must come after `ble::init`: writing internal
    // flash with the SoftDevice enabled is only legal through its own
    // `sd_flash_*` syscalls, which need the enabled SoftDevice.
    let shared_flash = leviculum_nrf::flash::shared_flash(sd);
    leviculum_nrf::radio_store::spawn_store_task(
        &spawner,
        shared_flash,
        t114::CONFIG.radio_config_flash_page,
    );
    // Telemetry-target persistence (#236): its own page (0xEA000, reserved
    // in memory.x beside identity and radio config, above the linker's
    // FLASH region and at the bootloader's USER_FLASH_END so it survives a
    // UF2 update), borrowing the same one-and-only SoftDevice flash handle.
    leviculum_nrf::telemetry::spawn_store_task(
        &spawner,
        shared_flash,
        t114::CONFIG.telemetry_flash_page,
    );

    // ST7789 status display — blind-driven (the panel is write-only, no
    // probe possible; see leviculum_nrf::st7789 module docs). Safe and
    // default-on for panel-less boards, so this single UF2 serves both
    // populations. The task owns the VEXT/VTFT power rails. The cfg only
    // exists because the workspace-wide clippy run also checks this bin
    // under the rak4631 feature set; the real t114 build always has it.
    #[cfg(feature = "bsp-t114")]
    {
        log_critical!("[STG] display-spawn");
        leviculum_nrf::st7789::init(
            &spawner,
            leviculum_nrf::st7789::TftWiring {
                spi: p.SPI3,
                sck: p.P1_08,
                mosi: p.P1_09,
                cs: p.P0_11,
                dc: p.P0_12,
                rst: p.P0_02,
                vtft: p.P0_03,
                leda: p.P0_15,
            },
            identity_hash,
        );
        info!("display task spawned (ST7789 blind-drive)");
    }

    // Quectel L76K on UARTE0 (#69). Pin naming is from the MCU's side:
    // the MCU receives on P1.05 and transmits on P1.07. Beware the
    // Meshtastic reference (variants/nrf52840/heltec_mesh_node_t114/
    // variant.h): the comments on GPS_TX_PIN/GPS_RX_PIN (lines 177/178)
    // describe the opposite of what the code does — effective are lines
    // 182/183, `PIN_SERIAL1_RX = GPS_RX_PIN` (P1.05) as the CPU's RX and
    // `PIN_SERIAL1_TX = GPS_TX_PIN` (P1.07) as the CPU's TX. Follow the
    // code, not the comments. P1.02 is the standby control the driver
    // holds high, P1.04 the unused PPS. Power comes from the VEXT rail
    // raised above.
    #[cfg(feature = "gnss")]
    {
        leviculum_nrf::gnss::init(
            &spawner,
            leviculum_nrf::gnss::GnssWiring {
                uarte: p.UARTE0,
                timer: p.TIMER1,
                ppi_a: p.PPI_CH0,
                ppi_b: p.PPI_CH1,
                rx: p.P1_05.into(),
                tx: p.P1_07.into(),
                pps: p.P1_04.into(),
                standby: Some(p.P1_02.into()),
                module: leviculum_nrf::gnss::ModuleKind::QuectelL76k,
            },
        );
        info!("gnss task spawned (L76K)");
    }

    let (hu, hf) = leviculum_nrf::heap_stats();
    info!("heap u={} f={}", hu, hf);
    leviculum_nrf::log_stack("post-init");

    // Interface adapters
    let serial_ctl_tx = serial.outgoing_tx;
    let mut serial_iface = EmbeddedInterface::new(serial.outgoing_tx);
    let mut lora_iface = LoRaInterface::new(lora_channels.outgoing_tx);
    let mut ble_iface = BleInterface::new(ble_channels.outgoing_tx);
    let ifac_configs: BTreeMap<usize, IfacConfig> = BTreeMap::new();

    // Boot blink
    led.set_level(Level::Low);
    for _ in 0..12_000_000u32 {
        cortex_m::asm::nop();
    }
    led.set_level(Level::High);

    // Telemetry (Codeberg #236). The delivery destination is registered
    // unconditionally, target or not: it is what a receiver verifies our
    // LXMF signature against, and it is useful on its own — a node that
    // announces it can be addressed by name instead of by hex string.
    let delivery_hash = leviculum_nrf::telemetry::register_delivery_destination(&mut node);
    if let Some(hash) = delivery_hash.as_ref() {
        let dh = hash.as_bytes();
        leviculum_nrf::log::log_fmt("[IDENTITY] ", format_args!(
            "t114_lxmf_delivery={:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            dh[0], dh[1], dh[2], dh[3], dh[4], dh[5], dh[6], dh[7],
            dh[8], dh[9], dh[10], dh[11], dh[12], dh[13], dh[14], dh[15]
        ));
    }
    // All three hashes a prober needs, on one boot-critical line (and
    // from here on in the periodic banner and the identity query).
    leviculum_nrf::identity::note_boot_identity(
        *node.identity().hash(),
        node.probe_dest_hash().map(|h| *h.as_bytes()),
        delivery_hash.as_ref().map(|h| *h.as_bytes()),
    );
    // The announce occasions telemetry does not cover (#376): a new BLE
    // peer, and the plain timer. Armed from the node's clock so the first
    // periodic announce is a fixed delay after boot, not after whatever
    // the loop happened to do first.
    let mut announce_gate = leviculum_nrf::announce::AnnounceGate::new(node.now_ms());
    let mut reporter = delivery_hash.map(leviculum_nrf::telemetry::Reporter::new);
    if reporter.is_none() {
        log_critical!("[TELEMETRY] target=00000000 state=off reason=no-delivery-destination");
    }
    if let Some(reporter) = reporter.as_mut() {
        // A persisted target comes back as awaiting-key unless its key
        // was persisted with it; the node then resolves it over the air
        // exactly as it would after a fresh set. The reporter rewrites a
        // hash-only record with the key once it is resolved (#370), so
        // after one successful resolution a reboot restores ready and
        // owes the immediate report.
        if let Some(stored) = leviculum_nrf::telemetry::load(t114::CONFIG.telemetry_flash_page) {
            reporter.apply_target(&mut node, stored);
        }
        // A persisted fixed position replaces the sensor from the first
        // report of this boot on — the pin must not depend on which
        // record the host set last.
        if let Some(stored) =
            leviculum_nrf::telemetry::load_fixed_position(t114::CONFIG.telemetry_flash_page)
        {
            reporter.apply_fixed_position(Some(stored));
        }
        reporter.log_banner();
    }
    let telemetry_target_rx = leviculum_nrf::telemetry::inbound_target_receiver();
    let fixed_position_rx = leviculum_nrf::telemetry::inbound_fixed_position_receiver();

    // Calendar seeding from GNSS (Codeberg #166 item 1, #69 on this
    // board): the main loop owns the node, so it is the one place a fix
    // can reach the wall-time seam. One accepted fix seeds; the monotonic
    // clock carries the calendar from there — the receiver keeps running
    // for position only, never as a clock.
    #[cfg(feature = "gnss")]
    let mut gnss_rx = {
        let rx = leviculum_nrf::baseboard::GNSS_FIX.receiver();
        if rx.is_none() {
            log_critical!("[TIME_SEED_REFUSED] source=gnss reason=watch_capacity");
        }
        rx
    };
    #[cfg(feature = "gnss")]
    let mut time_seed_gate = leviculum_gnss_time::SeedGate::new();

    // Periodic `[TRANSPORT]` counters (#344). Rides the main loop rather than
    // a spawned task: the counters live in the node this loop owns.
    let mut transport_stats = leviculum_nrf::transport_stats::Ticker::new();

    log_critical!("[STG] main-loop");
    leviculum_nrf::boot_trace::phase(leviculum_nrf::boot_trace::Phase::MainLoop);
    // Event-driven main loop, nine event sources:
    // 1. Serial incoming (USB)
    // 2. LoRa incoming (radio)
    // 3. BLE incoming (defragmented Reticulum packets from phone)
    // 4. Timer deadline (protocol maintenance, announces)
    // 5. GNSS time candidate (until the calendar is seeded once)
    // 6. Host wall-time injection (#238 control envelope)
    // 7. Host telemetry target (#238 control envelope, #236)
    // 8. Host fixed position (#238 control envelope)
    // 9. Telemetry evaluation tick (only while a target is configured)
    loop {
        transport_stats.poll(&node);
        // Clamped by the stats deadline so the line is still emitted on a
        // channel quiet enough that the node itself has nothing scheduled.
        let deadline = node
            .next_deadline()
            .map(Instant::from_millis)
            .unwrap_or(Instant::MAX)
            .min(transport_stats.deadline());

        let gnss_time_candidate = async {
            #[cfg(feature = "gnss")]
            {
                if time_seed_gate.is_seeded() {
                    // Seeded for this boot: nothing left to wait for.
                    core::future::pending::<u64>().await
                } else {
                    match gnss_rx.as_mut() {
                        Some(rx) => loop {
                            let fix = rx.changed().await;
                            if let Some(unix) = time_seed_gate.offer(fix.unix_secs) {
                                break unix;
                            }
                        },
                        None => core::future::pending::<u64>().await,
                    }
                }
            }
            #[cfg(not(feature = "gnss"))]
            {
                core::future::pending::<u64>().await
            }
        };

        // The tick rate is also the retry rate for a report the radio
        // could not take: the policy re-arms until a send is confirmed.
        // With no target configured there is nothing to wake up for.
        let telemetry_tick = async {
            match reporter.as_ref() {
                Some(reporter) if !reporter.is_off() => Timer::after(TELEMETRY_TICK_INTERVAL).await,
                _ => core::future::pending::<()>().await,
            }
        };

        // The periodic announce (#376 item 2). Sleeps exactly to its own
        // deadline rather than polling: an idle board with telemetry off
        // wakes twice an hour for this, and a clockless one once a
        // minute until its clock arrives.
        let announce_tick = async {
            Timer::after(Duration::from_millis(
                announce_gate.periodic_wait_ms(node.now_ms()).max(1),
            ))
            .await
        };

        let wake = select4(
            select4(
                serial.incoming_rx.receive(),
                lora_channels.incoming_rx.receive(),
                select(
                    ble_channels.incoming_rx.receive(),
                    ble_channels.peer_event_rx.receive(),
                ),
                Timer::at(deadline),
            ),
            gnss_time_candidate,
            select4(
                serial.wall_time_rx.receive(),
                telemetry_target_rx.receive(),
                fixed_position_rx.receive(),
                serial.announce_rx.receive(),
            ),
            select(telemetry_tick, announce_tick),
        )
        .await;
        // Mirror each interface's is_online() into the core (#365): a
        // path over an offline interface is no path, so a report due
        // while a carrier is off (or peerless, for BLE) falls through
        // to a path request instead of a silent carrier-off drop. After
        // the await, not at the loop top: a `--set-media ble=off` lands
        // in another task while this one sleeps, and the arm below must
        // route with the state as it is NOW.
        node.set_interface_online(0, serial_iface.is_online());
        node.set_interface_online(1, lora_iface.is_online());
        node.set_interface_online(2, ble_iface.is_online());
        // The peer-count sibling of the online mirror (Codeberg #365):
        // marks BLE as a peer-link carrier the transport may
        // re-originate an unanswerable path request on. Serial and LoRa
        // mirror nothing — no "peers" at this layer — and stay at zero.
        node.set_interface_peer_count(2, ble_iface.peer_count());
        match wake {
            Either4::Second(unix) => {
                // A GNSS fix carrying UTC. The seam applies the same
                // sanity window as every other time source; a refusal is
                // surfaced as a structured event, never swallowed.
                #[cfg(feature = "gnss")]
                {
                    use leviculum_core::transport::TimeSource;
                    if node.set_wall_time_unix_secs(unix, TimeSource::Gnss) {
                        time_seed_gate.mark_seeded();
                        leviculum_nrf::set_time_source(TimeSource::Gnss);
                        log_critical!("[TIME_SEED] source=gnss unix={}", unix);
                        log_critical!("[TIME_SOURCE] source={}", leviculum_nrf::time_source_str());
                    } else {
                        log_critical!("[TIME_SEED_REFUSED] source=gnss unix={}", unix);
                    }
                }
                #[cfg(not(feature = "gnss"))]
                let _ = unix;
            }
            Either4::Third(Either4::First(unix_secs)) => {
                // A host that knows wall time (#238 TYPE_WALL_TIME). The
                // seam applies the same sanity window as every other time
                // source; the bool picks the enveloped ack or the named
                // refusal — mirror of the GNSS path.
                use leviculum_core::envelope;
                use leviculum_core::transport::TimeSource;
                let answer = if node.set_wall_time_unix_secs(unix_secs, TimeSource::Host) {
                    leviculum_nrf::set_time_source(TimeSource::Host);
                    log_critical!("[TIME_SEED] source=host unix={}", unix_secs);
                    log_critical!("[TIME_SOURCE] source={}", leviculum_nrf::time_source_str());
                    envelope::encode_ack(envelope::TYPE_WALL_TIME)
                } else {
                    log_critical!("[TIME_SEED_REFUSED] source=host unix={}", unix_secs);
                    envelope::encode_refusal(envelope::TYPE_WALL_TIME, envelope::REFUSE_VALUE)
                };
                // Best effort: a full outgoing channel means a busy link;
                // the host's retry covers it.
                let _ = serial_ctl_tx.try_send(answer);
            }
            Either4::Third(Either4::Second(wire)) => {
                // A host set or cleared the telemetry target (#236). What
                // happens here is the part that needs the node — the
                // identity lookup that decides ready vs awaiting-key. The
                // persist and the answer both belong to the serial task
                // now: the answer may not go out before the record is on
                // the page (#358), and only the task that requested the
                // save can wait for it.
                use leviculum_nrf::telemetry::TargetOutcome;
                if let Some(reporter) = reporter.as_mut() {
                    match reporter.apply_target(&mut node, wire) {
                        TargetOutcome::Set(_) | TargetOutcome::Cleared => {
                            reporter.log_banner();
                        }
                    }
                }
            }
            Either4::Third(Either4::Third(position)) => {
                // A host set or cleared the fixed position. This is the
                // part that needs the reporter — the source switch and the
                // confirmation re-arm; the persist is the serial task's,
                // for the reason above.
                if let Some(reporter) = reporter.as_mut() {
                    reporter.apply_fixed_position(position);
                    match position {
                        Some(p) => log_critical!(
                            "[TELEMETRY] fixed-position set lat_e6={} lon_e6={} alt_e2={} alt_present={}",
                            p.latitude_e6,
                            p.longitude_e6,
                            p.altitude_e2.unwrap_or(0),
                            p.altitude_e2.is_some() as u8
                        ),
                        None => log_critical!("[TELEMETRY] fixed-position cleared"),
                    }
                }
            }
            Either4::Third(Either4::Fourth(())) => {
                // A host asked for an announce now (#376 `TYPE_ANNOUNCE`,
                // `lnflash --announce`). Exactly the announce the
                // telemetry path sends before a report — same
                // destination, same app data, same clock gate — so the
                // desk measures the announce the board sends on its own
                // cadence, only sooner.
                use leviculum_core::envelope;
                let answer = match delivery_hash.as_ref() {
                    None => {
                        // No delivery destination was registered this
                        // boot: nothing exists to announce, and neither a
                        // retry nor the clock can change that.
                        envelope::encode_refusal(
                            envelope::TYPE_ANNOUNCE,
                            envelope::REFUSE_UNSUPPORTED,
                        )
                    }
                    Some(_) if !node.has_plausible_wall_clock() => {
                        // The telemetry path's clock gate: the emission
                        // timestamp inside the announce is what peers
                        // rank paths by, and an uptime-stamped announce
                        // would poison the very path under measurement.
                        log_critical!("[ANNOUNCE] withheld reason=no-clock");
                        envelope::encode_refusal(envelope::TYPE_ANNOUNCE, envelope::REFUSE_NO_CLOCK)
                    }
                    Some(hash) => {
                        let app_data = leviculum_nrf::telemetry::announce_app_data(node.identity());
                        match node.announce_destination(hash, Some(&app_data)) {
                            Ok(out) => {
                                let mut ifaces: [&mut dyn Interface; 3] =
                                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                                let dispatched =
                                    dispatch_actions(&mut ifaces, out.actions, &ifac_configs);
                                leviculum_nrf::dispatch::settle(
                                    "announce-host",
                                    &mut node,
                                    &dispatched,
                                );
                                let dh = hash.as_bytes();
                                log_critical!(
                                    "[ANNOUNCE] sent dst={:02x}{:02x}{:02x}{:02x} reason=host",
                                    dh[0],
                                    dh[1],
                                    dh[2],
                                    dh[3]
                                );
                                envelope::encode_ack(envelope::TYPE_ANNOUNCE)
                            }
                            Err(_) => envelope::encode_refusal(
                                envelope::TYPE_ANNOUNCE,
                                envelope::REFUSE_BUSY,
                            ),
                        }
                    }
                };
                // Best effort, like the wall-time answer: a full outgoing
                // channel means a busy link; the host's retry covers it.
                let _ = serial_ctl_tx.try_send(answer);
            }
            Either4::Fourth(Either::First(())) => {
                // Telemetry evaluation (#236). Everything decided here is
                // decided in the policy crate; this arm reads the board's
                // sensors, hands them over, and dispatches whatever came
                // back.
                if let Some(reporter) = reporter.as_mut() {
                    let now_ms = node.now_ms();
                    let (readings, has_fix) = collect_readings(&node, sd);
                    let actions = reporter.tick(&mut node, now_ms, has_fix, &readings);
                    if !actions.is_empty() {
                        let mut ifaces: [&mut dyn Interface; 3] =
                            [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                        let dispatched = dispatch_actions(&mut ifaces, actions, &ifac_configs);
                        // The reporter settles first: it is the only caller
                        // that owns a cadence the dispatch's verdict decides
                        // (#344). `settle` counts and logs afterwards, as at
                        // every other site.
                        reporter.note_dispatch(&dispatched);
                        leviculum_nrf::dispatch::settle("telemetry", &mut node, &dispatched);
                    }
                }
            }
            Either4::Fourth(Either::Second(())) => {
                // The periodic announce (#376 item 2). Independent of
                // telemetry: a node that reports rarely, or has no target
                // at all, still has to be findable, and a phone that
                // missed the peer-up announce gets one from here. The gate
                // owns the deadline and the clock rule; this arm dispatches
                // whatever it hands back, which is nothing before the
                // deadline and nothing without a plausible clock.
                let actions = announce_gate.periodic(&mut node, delivery_hash.as_ref());
                if !actions.is_empty() {
                    let mut ifaces: [&mut dyn Interface; 3] =
                        [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                    let dispatched = dispatch_actions(&mut ifaces, actions, &ifac_configs);
                    leviculum_nrf::dispatch::settle("announce-periodic", &mut node, &dispatched);
                }
            }
            Either4::First(Either4::First(data)) => {
                info!("SER RX {} bytes", data.len());
                let output = node.handle_packet(InterfaceId(0), &data);
                info!("SER RX -> {} actions", output.actions.len());
                // An inbound TELEMETRY_REQUEST may ride any carrier (#371).
                if let Some(reporter) = reporter.as_mut() {
                    let now_ms = node.now_ms();
                    reporter.handle_inbound_events(&node, &output.events, now_ms);
                }
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                leviculum_nrf::dispatch::settle("ser-rx", &mut node, &dispatched);
            }
            Either4::First(Either4::Second(data)) => {
                // A medium switched off at runtime stops carrying traffic
                // in BOTH directions from the moment the frame was
                // answered: the interface drops what the core hands it,
                // this drops what the medium hands up. Without the second
                // half a "LoRa off" node would still deliver over LoRa,
                // which is precisely the masking the profile exists to
                // remove.
                if !leviculum_nrf::media::lora_active() {
                    continue;
                }
                let output = node.handle_packet(InterfaceId(1), &data);
                if !output.actions.is_empty() {
                    info!("LORA RX -> {} actions", output.actions.len());
                }
                if let Some(reporter) = reporter.as_mut() {
                    let now_ms = node.now_ms();
                    reporter.handle_inbound_events(&node, &output.events, now_ms);
                }
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                leviculum_nrf::dispatch::settle("lora-rx", &mut node, &dispatched);
            }
            Either4::First(Either4::Third(Either::First((peer, data)))) => {
                // The reception itself is already on the log: columba's
                // `BLE: RX <n>B conn=<h> frags=<k>` line names the link
                // and the peer's fragmentation (#376), so a second
                // per-packet line here said less and doubled the noise.
                // See the LoRa arm: a medium switched off at runtime
                // delivers nothing upward either.
                if !leviculum_nrf::media::ble_active() {
                    continue;
                }
                // Name the ingress link when the handshake did (#365):
                // paths learned through it become attributable to the
                // peer when the link dies.
                let output = match peer {
                    Some(peer) => node.handle_packet_from_peer(InterfaceId(2), peer, &data),
                    None => node.handle_packet(InterfaceId(2), &data),
                };
                if !output.actions.is_empty() {
                    info!("BLE RX -> {} actions", output.actions.len());
                }
                if let Some(reporter) = reporter.as_mut() {
                    let now_ms = node.now_ms();
                    reporter.handle_inbound_events(&node, &output.events, now_ms);
                }
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                leviculum_nrf::dispatch::settle("ble-rx", &mut node, &dispatched);
            }
            Either4::First(Either4::Third(Either::Second(event))) => {
                // A BLE peer transition (Codeberg #365). Lost: cull the
                // paths whose next hop is that peer so the next report
                // re-resolves over a carrier that can still deliver.
                // Up: pull the peer's delivery path over the fresh link
                // (Columba answers a path request but announces on
                // neither connect nor reconnect). Not gated on
                // media::ble_active — the cull is state cleanup, and a
                // pull for a medium switched off mid-run dies in the
                // interface's carrier-off drop like any other packet.
                let (label, output) = match event {
                    BlePeerEvent::Lost(peer) => {
                        let output = node.handle_interface_peer_lost(InterfaceId(2), peer);
                        info!(
                            "BLE peer lost, {} paths culled",
                            output
                                .events
                                .iter()
                                .filter(|e| matches!(e, NodeEvent::PathLost { .. }))
                                .count()
                        );
                        ("ble-peer-lost", output)
                    }
                    BlePeerEvent::Up(peer) => {
                        let mut output = node.handle_interface_peer_up(InterfaceId(2), peer);
                        info!("BLE peer up, {} pull actions", output.actions.len());
                        // #376: and announce OURSELVES to that peer, on
                        // its link alone. The peer just finished its
                        // identity handshake, so it can receive and it is
                        // exactly the node that does not know us; the pull
                        // above only asks what IT is. A broadcast here
                        // would reach the neighbour board, which forwards
                        // it, and the relayed copy racing the direct one
                        // is the two-hop reading this issue opened with.
                        output.actions.extend(announce_gate.peer_up(
                            &mut node,
                            delivery_hash.as_ref(),
                            peer,
                        ));
                        ("ble-peer-up", output)
                    }
                };
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                leviculum_nrf::dispatch::settle(label, &mut node, &dispatched);
            }
            Either4::First(Either4::Fourth(())) => {
                let output = node.handle_timeout();
                if !output.actions.is_empty() {
                    info!("timeout: {} actions", output.actions.len());
                }
                // A receipt timeout surfaces here, not on a reception —
                // the proof wait (#373) needs this arm's events too.
                if let Some(reporter) = reporter.as_mut() {
                    let now_ms = node.now_ms();
                    reporter.handle_inbound_events(&node, &output.events, now_ms);
                }
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                leviculum_nrf::dispatch::settle("timeout", &mut node, &dispatched);
            }
        }
    }
}

/// How often the telemetry policy is asked whether a report is due.
///
/// This is a *poll* rate, not a cadence: the cadence lives in the profile
/// and is minutes to hours. Five seconds is fine enough that "report now"
/// means now to an operator watching a serial log, and coarse enough that
/// it is invisible next to the tasks already waking on the same period.
const TELEMETRY_TICK_INTERVAL: Duration = Duration::from_secs(5);

/// Read this board's sensors for one telemetry evaluation.
///
/// Returns the readings and whether GNSS presence is `Fix` — only that
/// state may contribute a position (#240), and the policy is told
/// separately rather than having to infer it from the numbers.
///
/// The per-board part of telemetry is exactly this function: which
/// peripherals exist. The T114 has the L76K (#69) and no battery gauge
/// (the ADC has no task), so a fix contributes position, speed, bearing
/// and the HDOP the accuracy gate reads, and the battery field stays
/// empty.
///
/// The die temperature it does have: every nRF52840 carries one and the
/// SoftDevice is enabled on both boards, so it is read here through the
/// only legal path ([`leviculum_nrf::telemetry::die_temperature_quarter_c`]).
fn collect_readings<R, C, S>(
    node: &leviculum_core::node::NodeCore<R, C, S>,
    sd: &nrf_softdevice::Softdevice,
) -> (leviculum_nrf::telemetry::Readings, bool)
where
    R: rand_core::CryptoRngCore,
    C: leviculum_core::traits::Clock,
    S: leviculum_core::traits::Storage,
{
    // Without the gnss feature nothing mutates this; the die temperature
    // is set in the initialiser, so it does not lift the gate.
    #[cfg_attr(not(feature = "gnss"), allow(unused_mut, clippy::let_and_return))]
    let mut readings = leviculum_nrf::telemetry::Readings {
        // A timebase below the plausibility floor is uptime seconds, not a
        // calendar estimate — the anchor model's "never ahead" rule has
        // nothing to work with there. Which arm anchored it is reported
        // alongside every reading as `[TIME_SOURCE]`.
        unix_secs: node
            .has_plausible_wall_clock()
            .then(|| node.emission_secs()),
        die_temperature_quarter_c: leviculum_nrf::telemetry::die_temperature_quarter_c(sd),
        ..Default::default()
    };
    #[cfg(not(feature = "gnss"))]
    let has_fix = false;
    #[cfg(feature = "gnss")]
    let has_fix = {
        use leviculum_nrf::baseboard::{GnssPresence, GNSS_FIX, GNSS_PRESENCE};
        if let Some(fix) = GNSS_FIX.try_get() {
            readings.latitude = fix.latitude;
            readings.longitude = fix.longitude;
            readings.altitude_m = fix.altitude_m;
            readings.speed_mps = fix.speed_mps;
            readings.bearing_deg = fix.bearing_deg;
            readings.hdop = fix.hdop;
        }
        matches!(
            GNSS_PRESENCE.try_get().map(|p| p.state),
            Some(GnssPresence::Fix)
        )
    };
    (readings, has_fix)
}

#[embassy_executor::task]
async fn boot_log_repeater(initial_len: usize) {
    for _ in 0..6 {
        Timer::after(Duration::from_secs(10)).await;
        info!("[BOOT] path_table_initial_len={}", initial_len);
    }
}

/// Re-emit the firmware build banner periodically so a debug-serial
/// reader can verify the running git_sha at any time, not only inside the
/// short boot window. The CI auto-flash verify reads this back. Uses the
/// embassy time driver (same timing infra the other periodic tasks use);
/// no SD-reserved peripheral is touched directly.
///
/// The `[MEDIA]` and `[NAME ]` lines ride along for the same reason and
/// read their state live, so a runtime change shows up here within five
/// seconds and a capture attached after the boot window still learns
/// which carriers this board is on and what it is called.
#[embassy_executor::task]
async fn fw_build_banner(media_src: leviculum_nrf::media::Source) {
    loop {
        Timer::after(Duration::from_secs(5)).await;
        log_critical!("[FW_BUILD] {}", leviculum_nrf::FW_BUILD_STAMP);
        log_critical!("[TIME_SOURCE] source={}", leviculum_nrf::time_source_str());
        leviculum_nrf::media::log_banner(media_src);
        leviculum_nrf::name::log_banner();
        leviculum_nrf::identity::log_banner();
    }
}
