//! Lifecycle handle for a runtime-attached byte-channel interface.

use leviculum_core::transport::InterfaceId;
use tokio::sync::oneshot;

/// Handle to a runtime-attached byte-channel interface.
///
/// **Hold it to keep the interface attached; drop it (or call
/// [`detach`](Self::detach)) to detach** — the interface task stops, its
/// channel closes, and the event loop removes it from routing.
#[must_use = "dropping the handle immediately detaches the interface"]
pub struct ByteChannelHandle {
    id: InterfaceId,
    shutdown: Option<oneshot::Sender<()>>,
}

impl ByteChannelHandle {
    pub(crate) fn new(id: InterfaceId, shutdown: oneshot::Sender<()>) -> Self {
        Self {
            id,
            shutdown: Some(shutdown),
        }
    }

    /// The interface id the node assigned to this channel.
    #[must_use]
    pub fn id(&self) -> InterfaceId {
        self.id
    }

    /// Detach the interface now, instead of on drop.
    pub fn detach(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for ByteChannelHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}
