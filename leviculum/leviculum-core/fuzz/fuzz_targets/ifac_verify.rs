#![no_main]
//! Fuzz `Ifac::verify_ifac` / `has_ifac_flag`, the IFAC (interface access
//! code) authentication decoder run on every inbound packet on an IFAC-guarded
//! interface, before the packet parser. Fed attacker bytes; must reject
//! malformed input with `Err`, never panic / overflow / index out of bounds.
use leviculum_core::ifac::IfacConfig;
use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;

static IFAC: OnceLock<IfacConfig> = OnceLock::new();

fuzz_target!(|data: &[u8]| {
    let ifac = IFAC.get_or_init(|| {
        IfacConfig::new(Some("fuzz-net"), Some("fuzz-key"), 8).expect("build fuzz IFAC")
    });
    let _ = IfacConfig::has_ifac_flag(data);
    let _ = ifac.verify_ifac(data);
});
