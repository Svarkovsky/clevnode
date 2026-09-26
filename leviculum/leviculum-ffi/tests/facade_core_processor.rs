//! A core processor written against `leviculum_std::api` and nothing else.
//!
//! It lives here rather than in `leviculum-std`, where every path is reachable
//! and nothing about the public surface is proven: this package depends on
//! `leviculum-std` alone, so a gap in the projection is a build error. Same
//! argument as `leviculum-lxmf-node` (#196 part B).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use leviculum_std::api::{
    generate_identity, CoreProcessor, NodeBuilder, NodeEvent, StdNodeCore, TickOutput,
};

/// Counts hook calls. Holds no node handle, as the seam requires: a hook runs
/// under the core lock, and anything derived from `ReticulumNode` would
/// deadlock on it.
struct CountHooks(Arc<AtomicUsize>);

impl CoreProcessor for CountHooks {
    fn on_event(&mut self, _core: &mut StdNodeCore, _event: &NodeEvent) -> TickOutput {
        self.0.fetch_add(1, Ordering::Relaxed);
        TickOutput::empty()
    }

    fn on_tick(&mut self, _core: &mut StdNodeCore, _now_ms: u64) -> TickOutput {
        self.0.fetch_add(1, Ordering::Relaxed);
        TickOutput::empty()
    }
}

#[tokio::test]
async fn a_processor_installed_through_the_facade_is_driven() {
    let calls = Arc::new(AtomicUsize::new(0));
    let storage = tempfile::tempdir().expect("temp dir");
    let mut node = NodeBuilder::new()
        .identity(generate_identity())
        .storage_path(storage.path().to_path_buf())
        .enable_transport(false)
        .core_processor(CountHooks(calls.clone()))
        .build()
        .expect("build");
    node.start().await.expect("start");

    // The driver's idle cadence is one second, so three of them is room enough
    // for the timer branch to have run on a loaded machine.
    for _ in 0..30 {
        if calls.load(Ordering::Relaxed) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    node.stop().await.expect("stop");

    assert!(
        calls.load(Ordering::Relaxed) > 0,
        "the driver never called the processor"
    );
}
