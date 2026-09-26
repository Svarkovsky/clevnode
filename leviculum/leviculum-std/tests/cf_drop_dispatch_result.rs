//! Compile-fail fixture: a `DispatchResult` may not be dropped on the floor
//! (Codeberg #344).
//!
//! Empty unless the internal `__compile_fail_fixtures` feature is on. The
//! check that this does NOT build is `scripts/check-processor-compile-fail.sh`,
//! run by `just fast`.
//!
//! Pinned diagnostic: the `unused_must_use` lint, message `unused
//! \`DispatchResult\` that must be used`. Unlike the other fixtures here this
//! is a LINT, not an `error[E…]`: `#[must_use]` is a warning by default and
//! only the `deny` below turns it fatal. The gate therefore requires that NO
//! `error[E…]` code is emitted for this fixture, so a fixture that broke for
//! an unrelated reason (a renamed import, a changed signature) is caught
//! instead of silently keeping the claim alive.
//!
//! Why it matters: `dispatch_actions` returns the retries the core asked for,
//! the interface errors it saw, and the actions it could not route. All nine
//! firmware call sites discarded that value as a bare statement, which turned
//! a `BufferFull` on the LoRa interface into a packet that was neither
//! retried, nor counted, nor logged. The attribute is what makes the compiler
//! find the tenth site; this fixture is what proves the attribute is still on.

#![cfg(feature = "__compile_fail_fixtures")]
#![deny(unused_must_use)]

use leviculum_core::ifac::IfacConfig;
use leviculum_core::traits::Interface;
use leviculum_core::transport::dispatch_actions;
use std::collections::BTreeMap;

fn floor(interfaces: &mut [&mut dyn Interface], ifac_configs: &BTreeMap<usize, IfacConfig>) {
    // The exact shape every firmware call site had before #344.
    dispatch_actions(interfaces, Vec::new(), ifac_configs);
}
