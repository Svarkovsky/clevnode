//! Interoperability tests with Python Reticulum.
//!
//! These tests verify that our Rust implementation correctly interoperates
//! with the Python Reticulum reference implementation. Tests are organized
//! into two categories:
//!
//! ## Test Categories
//!
//! 1. **Unit Tests** - Pure logic tests that don't require a daemon:
//!    - `protocol_tests` - Flag encoding, hash derivation, packet layout
//!
//! 2. **Interop Tests** - Tests using the TestDaemon infrastructure:
//!    - `announce_interop_tests` - Core announce/path tests
//!    - `discovery_tests` - Bidirectional discovery tests
//!    - `link_tests` - Link establishment tests (Rust as initiator)
//!    - `responder_tests` - Link responder tests (Rust as responder)
//!    - `flow_tests` - End-to-end flow tests
//!
//! ## Running Tests
//!
//! ```sh
//! # Run all interop tests (includes daemon tests that auto-spawn Python daemon)
//! cargo test --package leviculum-std --test rnsd_interop
//!
//! # Run with verbose output
//! cargo test --package leviculum-std --test rnsd_interop -- --nocapture
//!
//! # Run specific test module
//! cargo test --package leviculum-std --test rnsd_interop announce_interop_tests
//! cargo test --package leviculum-std --test rnsd_interop responder_tests
//! cargo test --package leviculum-std --test rnsd_interop protocol_tests
//! ```

mod announce_emission_interop_tests;
mod announce_interop_tests;
mod auto_interop_tests;
mod backbone_interop_tests;
mod blackhole_interop_tests;
mod ble_torn_announce_tests;
mod channel_tests;
mod common;
mod comprehensive_network_test;
mod discovery_interop_tests;
mod discovery_tests;
mod edge_case_tests;
mod encryption_tests;
mod explicit_hash_interop_tests;
mod flood_tests;
mod flow_tests;
mod group_crypto_tests;
mod harness;
mod held_announce_interop_tests;
mod ifac_interop_tests;
mod interface_mode_tests;
mod lifecycle_tests;
mod link_keepalive_close_tests;
mod link_manager_tests;
mod link_packet_interop_tests;
mod link_tests;
mod lnomad_fetch_interop_tests;
mod loadtest_tcp_hub_tests;
mod lrproof_hop_undercount_interop_tests;
mod lxmf_interop_tests;
mod mtu_tests;
mod multihop_tests;
mod node_api_tests;
mod nomad_page_tests;
mod path_gap_tests;
mod path_recovery_tests;
mod pipe_interop_tests;
mod plain_broadcast_tests;
mod probe_tests;
mod proof_tests;
mod protocol_tests;
mod python_parity_tests;
mod python_rnstatus_server_tests;
mod ratchet_rotation_tests;
mod ratchet_tests;
mod relay_integration_tests;
mod remote_management_server_tests;
mod remote_management_tests;
mod request_resource_interop_tests;
mod request_tests;
mod resource_tests;
mod responder_node_tests;
mod responder_tests;
mod response_resource_tests;
mod retain_interop_tests;
mod reverse_rpc_interop_tests;
mod rpc_interop_tests;
mod rust_relay_tests;
mod serial_interop_tests;
mod shared_instance_tests;
mod status_parity_tests;
mod stress_tests;
mod transport_interop_tests;
mod tunnel_interop_tests;
mod udp_interop_tests;
