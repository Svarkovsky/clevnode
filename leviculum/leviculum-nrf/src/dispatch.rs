//! What the board does with the answer `dispatch_actions` gives it
//! (Codeberg #344).
//!
//! `leviculum_core::transport::dispatch_actions` returns a `DispatchResult`
//! naming three kinds of loss: frames the core wants re-sent, errors the
//! interfaces reported, and actions it could not route at all. Every firmware
//! call site used to discard that value as a bare statement, so a full
//! `LORA_OUTGOING` queue produced a packet that was neither retried, nor
//! counted, nor logged — the core worked out that the frame needed re-sending
//! and the answer went on the floor. `DispatchResult` is `#[must_use]` now, so
//! the compiler finds any site that tries again; this module is what a site
//! does instead of dropping it.
//!
//! Deliberately NOT a retry: whether a dropped frame is re-sent, and by whom,
//! is a design decision that needs the number this line produces. A retry loop
//! added here would hide exactly the count it is meant to expose.

use leviculum_core::node::NodeCore;
use leviculum_core::traits::{Clock, Storage};
use leviculum_core::transport::DispatchResult;
use rand_core::CryptoRngCore;

/// Account for and report one dispatch.
///
/// `site` names the main-loop arm the dispatch belongs to, so a line in a
/// capture says which event produced the loss without a timestamp comparison.
///
/// Silent when the dispatch lost nothing — the common case, and the one that
/// must stay quiet: a line per successful dispatch on a busy mesh is the same
/// as no line at all. `[DISPATCH_LOSS]` appearing in a capture is therefore
/// itself the signal, before anybody reads the numbers on it.
///
/// ```text
/// [DISPATCH_LOSS] site=lora-rx errors=1 retries=1 unrouted=0 err=iface1/buffer full
/// ```
pub fn settle<R, C, S>(site: &str, node: &mut NodeCore<R, C, S>, result: &DispatchResult)
where
    R: CryptoRngCore,
    C: Clock,
    S: Storage,
{
    // Unroutable actions are decided below the node, which cannot reach its
    // own counters from there; folding them in here keeps `packets_dropped`
    // accounting for every packet this board threw away.
    node.record_dispatch_drops(result);
    if let Some(loss) = result.loss() {
        crate::log::log_fmt("[DISPATCH_LOSS] ", format_args!("site={} {}", site, loss));
    }
}
