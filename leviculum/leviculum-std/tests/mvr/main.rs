//! Minimum-viable-reproduction (mvr) test tier.
//!
//! See CLAUDE.md §Protocol debugging discipline for the policy these tests
//! implement. Each file here isolates one named protocol-layer failure from
//! the full-scenario tests so that it runs deterministically in seconds
//! rather than minutes, with full structured event logs on failure.
//!
//! mvrs must not depend on LoRa hardware, Docker, or Python. When a full-
//! scenario bug reproduces over a non-LoRa transport, the mvr builds that
//! transport from process primitives and holds the rest of the protocol
//! stack (daemon, client tools, resource machinery) unchanged.

// Shared harness — loaded once here so all mvr test files can reach
// it via `use crate::harness::...`.  Loading it once avoids the
// `clippy::duplicate_mod` warning that fires when each mvr file
// pulls in `#[path] mod harness;` independently.
#[path = "../rnsd_interop/harness.rs"]
#[allow(dead_code)]
pub mod harness;

mod announce_emission_unix_time;
mod client_wait_for_path_request_fallback;
mod core_lock_reentrancy;
mod core_processor_seam;
mod iface_online_after_task_death;
mod link_failure_recovery_silent_resume;
mod lncp_fetch_rust_responder;
mod ratchet_rotation_single_packet;
mod resource_consecutive_push_window_policy;
mod responder_close_delivery;
mod rust_client_path_install_from_python;
mod rust_client_path_install_loop_race;
mod rust_client_path_install_via_relay;
mod rust_client_path_install_with_own_echo;
mod udp_hostname_forward;
