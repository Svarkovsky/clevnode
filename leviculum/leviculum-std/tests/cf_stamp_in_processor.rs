//! Compile-fail fixture: the natural spelling of a peer-priced stamp search in
//! the hook does not compile (Codeberg #196, #185).
//!
//! Empty unless the internal `__compile_fail_fixtures` feature is on. The
//! check that this does NOT build, and fails with the error code named below,
//! is `scripts/check-processor-compile-fail.sh`, run by `just fast`.
//!
//! Pinned error: `error[E0728]: `await` is only allowed inside `async`
//! functions and blocks`.
//!
//! What this fixture establishes, and what it does not. A peer chooses the
//! stamp cost, and an announced cost of 254 is legal and effectively
//! unfinishable. `leviculum-lxmf` keeps that search off the core by making it
//! an `async fn` over a detached `StampExecutor`
//! (`leviculum-lxmf/src/router/stamp_runtime.rs`), and the way anyone would
//! actually write the mistake — call it, `.await` it — is what fails here.
//! That is a tripwire on the spelling, not a proof of the property. The
//! property is not available: `StampExecutor::generate`
//! (leviculum-lxmf/src/stamp.rs:74) returns `Pin<Box<dyn Future>>`, which any
//! synchronous `fn` can drive to completion with
//! `futures::executor::block_on`. That is one instance of the residual hole
//! `leviculum-std/src/driver/processor.rs` names in its module doc — the hole
//! is any hook body that re-enters the core mutex, and a `block_on` over an
//! executor that never touches the core is in fact the *benign* end of it: it
//! burns the lock rather than deadlocking on it. No signature closes either.
//!
//! E0728 here is therefore the same compiler property `cf_await_in_processor`
//! already pins, reached through the stamp API rather than through the
//! driver's. It is kept because the spelling is worth a tripwire of its own:
//! this is the call a consumer wiring `leviculum-lxmf` into the seam is most
//! likely to reach for.
//!
//! The actual defence against a peer-priced grind on the core lock is
//! `PROCESSOR_TICK_BUDGET`, which observes rather than prevents — the node
//! emits `CORE_PROCESSOR_OVER_BUDGET` and stays stalled until the hook
//! returns.

#![cfg(feature = "__compile_fail_fixtures")]

use leviculum_core::node::NodeEvent;
use leviculum_core::transport::TickOutput;
use leviculum_lxmf::{DeliveryStampRequest, StampExecutor};
use leviculum_std::driver::{CoreProcessor, StdNodeCore};

struct Grinder<E> {
    request: DeliveryStampRequest,
    executor: E,
}

impl<E: StampExecutor + Send + 'static> CoreProcessor for Grinder<E> {
    fn on_event(&mut self, _core: &mut StdNodeCore, _event: &NodeEvent) -> TickOutput {
        let _stamp = self.request.generate_with(&mut self.executor).await;
        TickOutput::empty()
    }
}
