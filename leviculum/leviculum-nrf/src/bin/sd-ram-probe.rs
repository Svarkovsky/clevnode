//! SoftDevice RAM-requirement probe (#255 T2 design batch).
//!
//! Asks the S140 what application RAM start each candidate BLE
//! configuration requires and prints the answers over the debug CDC
//! port. It never advertises, never scans, never starts the mesh
//! stack: each measurement is `sd_softdevice_enable` → `sd_ble_cfg_set`
//! series → `sd_ble_enable` against a deliberately undersized RAM base
//! → read the exact required base out of the `NRF_ERROR_NO_MEM` reply →
//! `sd_softdevice_disable`. One boot yields the whole cost curve, and
//! the BLE stack is never actually initialised.
//!
//! Fits-today is judged against `__sretained` (= ORIGIN(RETAINED) from
//! memory.x, the SoftDevice's ceiling: above it sit the cross-boot
//! records, then the flip-link stack) — NOT `__sdata`, which is what
//! nrf-softdevice's own enable-time check uses and which sits a whole
//! stack region higher: by that check a config can "fit" while the
//! SoftDevice reservation overlaps the retained records or our stack.
//!
//! This binary is a measurement instrument, not firmware: it is not
//! flashed by any recipe, and per the #255 T2 batch instruction it must
//! not go onto a rig board without the report saying so first.

#![no_std]
#![no_main]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_time::Timer;
use leviculum_nrf::log_critical;
use nrf_softdevice::raw;

#[cfg(feature = "bsp-rak4631")]
use leviculum_nrf::boards::rak4631 as board;
#[cfg(feature = "bsp-t114")]
use leviculum_nrf::boards::t114 as board;

/// One BLE configuration whose RAM requirement the probe measures.
/// Everything not listed here matches the shipped `ble::init` config:
/// `event_length: 24`, `adv_set_count: 1`, default attribute-table
/// size, no vendor UUIDs beyond the default pool.
struct Case {
    name: &'static str,
    conn_count: u8,
    periph_role_count: u8,
    central_role_count: u8,
    central_sec_count: u8,
    att_mtu: u16,
}

const CASES: [Case; 9] = [
    // Exactly the shipped config (ble.rs `init`): today's requirement,
    // hence today's headroom against the linked RAM ORIGIN.
    Case {
        name: "today",
        conn_count: 1,
        periph_role_count: 1,
        central_role_count: 0,
        central_sec_count: 0,
        att_mtu: 256,
    },
    // Same at the 23-byte default ATT MTU: isolates what att_mtu=256
    // costs per connection.
    Case {
        name: "mtu23",
        conn_count: 1,
        periph_role_count: 1,
        central_role_count: 0,
        central_sec_count: 0,
        att_mtu: 23,
    },
    // The #255 topology steps: one phone (peripheral slot) plus N
    // neighbour links we initiate (central slots).
    Case {
        name: "c1",
        conn_count: 2,
        periph_role_count: 1,
        central_role_count: 1,
        central_sec_count: 0,
        att_mtu: 256,
    },
    // As c1 but with one concurrent central pairing procedure allowed,
    // to price central_sec_count separately.
    Case {
        name: "c1sec",
        conn_count: 2,
        periph_role_count: 1,
        central_role_count: 1,
        central_sec_count: 1,
        att_mtu: 256,
    },
    Case {
        name: "c2",
        conn_count: 3,
        periph_role_count: 1,
        central_role_count: 2,
        central_sec_count: 0,
        att_mtu: 256,
    },
    Case {
        name: "c3",
        conn_count: 4,
        periph_role_count: 1,
        central_role_count: 3,
        central_sec_count: 0,
        att_mtu: 256,
    },
    // The #372 steps: N incoming (peripheral) links plus the one
    // initiated central link. p3c1 is the shipped configuration; these
    // two cases measure the periph-slot cost the memory.x extrapolation
    // (from c1's central-slot delta) only estimates.
    Case {
        name: "p2c1",
        conn_count: 3,
        periph_role_count: 2,
        central_role_count: 1,
        central_sec_count: 0,
        att_mtu: 256,
    },
    Case {
        name: "p3c1",
        conn_count: 4,
        periph_role_count: 3,
        central_role_count: 1,
        central_sec_count: 0,
        att_mtu: 256,
    },
    // The ble-reticulum reference ceiling (MAX_PEERS = 7).
    Case {
        name: "c7",
        conn_count: 8,
        periph_role_count: 1,
        central_role_count: 7,
        central_sec_count: 0,
        att_mtu: 256,
    },
];

/// What one measurement produced. `ret` is the raw NRF error code of
/// the call that decided the outcome; `wanted` is the app RAM base the
/// SoftDevice reported (0 if it never got that far).
#[derive(Clone, Copy)]
struct Reading {
    ret: u32,
    wanted: u32,
}

/// The SoftDevice calls this on an internal assertion. The probe never
/// starts BLE activity, so reaching it means the enable/disable cycle
/// itself faulted — worth a loud panic (the lib panic handler persists
/// it as a post-mortem).
unsafe extern "C" fn fault_handler(id: u32, pc: u32, info: u32) {
    panic!("SD fault id={id} pc={pc:#x} info={info:#x}");
}

/// The SoftDevice's ceiling: ORIGIN(RETAINED) from memory.x, the floor
/// of the cross-boot record region (the stack floor sits above that).
/// This is the address the SoftDevice reservation must stay below.
fn linked_ram_origin() -> u32 {
    unsafe extern "C" {
        static __sretained: u32;
    }
    core::ptr::addr_of!(__sretained) as u32
}

fn measure(case: &Case, linked_base: u32) -> Reading {
    // Same LF clock config as ble::init — the clock source does not
    // move the RAM requirement, but an enable that fails for clock
    // reasons would masquerade as a config problem.
    let clock = raw::nrf_clock_lf_cfg_t {
        source: raw::NRF_CLOCK_LF_SRC_RC as u8,
        rc_ctiv: 16,
        rc_temp_ctiv: 2,
        accuracy: raw::NRF_CLOCK_LF_ACCURACY_500_PPM as u8,
    };
    let ret = unsafe { raw::sd_softdevice_enable(&clock, Some(fault_handler)) };
    if ret != raw::NRF_SUCCESS {
        return Reading { ret, wanted: 0 };
    }

    // All conn_cfgs land under tag 1, the tag nrf-softdevice opens every
    // connection on (APP_CONN_CFG_TAG, softdevice.rs:68).
    let conn_gap = raw::ble_cfg_t {
        conn_cfg: raw::ble_conn_cfg_t {
            conn_cfg_tag: 1,
            params: raw::ble_conn_cfg_t__bindgen_ty_1 {
                gap_conn_cfg: raw::ble_gap_conn_cfg_t {
                    conn_count: case.conn_count,
                    event_length: 24,
                },
            },
        },
    };
    let conn_gatt = raw::ble_cfg_t {
        conn_cfg: raw::ble_conn_cfg_t {
            conn_cfg_tag: 1,
            params: raw::ble_conn_cfg_t__bindgen_ty_1 {
                gatt_conn_cfg: raw::ble_gatt_conn_cfg_t {
                    att_mtu: case.att_mtu,
                },
            },
        },
    };
    let role_count = raw::ble_cfg_t {
        gap_cfg: raw::ble_gap_cfg_t {
            role_count_cfg: raw::ble_gap_cfg_role_count_t {
                adv_set_count: 1,
                periph_role_count: case.periph_role_count,
                central_role_count: case.central_role_count,
                central_sec_count: case.central_sec_count,
                _bitfield_1: raw::ble_gap_cfg_role_count_t::new_bitfield_1(0),
            },
        },
    };

    // `sd_ble_cfg_set` may itself return NO_MEM against a small base;
    // like nrf-softdevice we let `sd_ble_enable` deliver the verdict and
    // only bail on errors that mean the config never registered.
    for (id, cfg) in [
        (raw::BLE_CONN_CFGS_BLE_CONN_CFG_GAP, &conn_gap),
        (raw::BLE_CONN_CFGS_BLE_CONN_CFG_GATT, &conn_gatt),
        (raw::BLE_GAP_CFGS_BLE_GAP_CFG_ROLE_COUNT, &role_count),
    ] {
        let ret = unsafe { raw::sd_ble_cfg_set(id, cfg, linked_base) };
        if ret != raw::NRF_SUCCESS && ret != raw::NRF_ERROR_NO_MEM {
            let _ = unsafe { raw::sd_softdevice_disable() };
            return Reading { ret, wanted: 0 };
        }
    }

    // Deliberately undersized base: forces NRF_ERROR_NO_MEM, on which the
    // S140 documentedly writes the exact required base into the in-out
    // parameter — and, crucially, never begins initialising the BLE stack.
    // Passing the honest linked base instead would rely on the (also
    // documented, but here unneeded) write-back-on-success path. Whether a
    // config fits today is `margin_vs_linked >= 0` in the output line.
    let mut wanted: u32 = 0x2000_2000;
    let ret = unsafe { raw::sd_ble_enable(&mut wanted) };
    let _ = unsafe { raw::sd_softdevice_disable() };
    Reading { ret, wanted }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut config = embassy_nrf::config::Config::default();
    config.hfclk_source = embassy_nrf::config::HfclkSource::ExternalXtal;
    config.gpiote_interrupt_priority = embassy_nrf::interrupt::Priority::P2;
    config.time_interrupt_priority = embassy_nrf::interrupt::Priority::P2;
    let p = embassy_nrf::init(config);

    leviculum_nrf::init_heap();
    let vbus = leviculum_nrf::init_vbus();
    let _serial = leviculum_nrf::usb::init(&spawner, p.USBD, vbus, &board::CONFIG);

    log_critical!("leviculum SD-RAM probe (#255 T2)");
    log_critical!("[FW_BUILD] {}", leviculum_nrf::FW_BUILD_STAMP);

    let base = linked_ram_origin();
    log_critical!("[PROBE] linked_ram_origin={base:#010x}");

    let mut results = [Reading {
        ret: u32::MAX,
        wanted: 0,
    }; CASES.len()];
    for (case, slot) in CASES.iter().zip(results.iter_mut()) {
        *slot = measure(case, base);
        // Let the disable settle before the next enable cycle.
        Timer::after_millis(100).await;
    }

    // Re-print forever so the numbers appear whenever the host attaches.
    loop {
        for (case, r) in CASES.iter().zip(results.iter()) {
            log_critical!(
                "SD_RAM_PROBE case={} conn={} periph={} central={} sec={} att_mtu={} ret={} want={:#010x} sd_bytes={} margin_vs_linked={}",
                case.name,
                case.conn_count,
                case.periph_role_count,
                case.central_role_count,
                case.central_sec_count,
                case.att_mtu,
                r.ret,
                r.wanted,
                r.wanted.wrapping_sub(0x2000_0000),
                i64::from(base) - i64::from(r.wanted),
            );
        }
        Timer::after_secs(5).await;
    }
}
