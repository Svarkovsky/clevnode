//! Compile-fail fixture: the handle a [`CoreProcessor`] is given carries no
//! route to the driver's dispatch channel (Codeberg #196).
//!
//! Empty unless the internal `__compile_fail_fixtures` feature is on. The
//! check that this does NOT build, and fails with the error code named below,
//! is `scripts/check-processor-compile-fail.sh`, run by `just fast`.
//!
//! Pinned error: `error[E0599]: no method named `packet_sender` found for
//! mutable reference `&mut NodeCore<...>``, and the same for `link_handle` and
//! `node` — all three are named in the gate, so a route that stopped being
//! refused fails it.
//!
//! Why that error: the hook is handed `&mut StdNodeCore`, the sans-io core.
//! Three types can reach `action_dispatch_tx` — `PacketSender`, `LinkHandle`,
//! `ReticulumNode` itself — and each is constructed from `ReticulumNode`,
//! never from the core. One attempt per type below; E0599 is the compiler
//! saying exactly that, three times over: the constructor is not on the thing
//! the seam hands out.
//!
//! This is the fixture with the widest claim of the three, and it is still
//! narrower than it looks. It proves the seam does not *hand out* a route back
//! into the node. It cannot prove a processor has none: a processor struct may
//! hold an `Arc<ReticulumNode>` injected after construction, and then
//! `self.node.has_path(..)` — no `.await`, no channel, plain safe code —
//! deadlocks on the core mutex the hook is already holding. See
//! `leviculum-std/src/driver/processor.rs` for what does hold that line up
//! (registration precedes the node's existence) and what does not (the type
//! system).
//!
//! Method calls rather than paths on purpose. `NodeCore` lives in
//! `leviculum-core`, which cannot name a `leviculum-std` type at all, so no
//! *inherent* method could ever return one of the three. What could is an
//! extension trait added here for convenience (`impl NodeCoreExt for
//! StdNodeCore`), which is a plausible future refactor and is exactly what
//! these three calls would then resolve to.

#![cfg(feature = "__compile_fail_fixtures")]

use leviculum_core::node::NodeEvent;
use leviculum_core::transport::TickOutput;
use leviculum_core::{DestinationHash, LinkId};
use leviculum_std::driver::{CoreProcessor, StdNodeCore};

struct Smuggler;

impl CoreProcessor for Smuggler {
    fn on_event(&mut self, core: &mut StdNodeCore, _event: &NodeEvent) -> TickOutput {
        let dest = DestinationHash::new([0u8; 16]);
        let link_id = LinkId::new([0u8; 16]);
        let _sender = core.packet_sender(&dest);
        let _link = core.link_handle(&link_id);
        let _node = core.node();
        TickOutput::empty()
    }
}
