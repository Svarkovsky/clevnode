//! Send-only async handle for single-packet destinations
//!
//! Provides a self-contained handle for fire-and-forget packet delivery
//! to a specific destination. The single-packet analog of LinkHandle.

use std::sync::{Arc, Mutex};

use crate::sync_ext::MutexRecover;

use tokio::sync::mpsc;

use leviculum_core::constants::TRUNCATED_HASHBYTES;
use leviculum_core::transport::TickOutput;
use leviculum_core::DestinationHash;

use super::StdNodeCore;
use crate::error::Error;

/// Async handle for sending single packets to a destination
///
/// `PacketSender` provides a self-contained handle for fire-and-forget
/// packet delivery to a specific destination hash. It is the single-packet
/// analog of [`super::LinkHandle`].
///
/// Created via [`super::ReticulumNode::packet_sender()`]. The handle
/// locks the core to build the packet and dispatches the resulting actions
/// through the event loop.
///
/// # Example
///
/// ```no_run
/// # use leviculum_std::driver::{ReticulumNodeBuilder, PacketSender};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let node = ReticulumNodeBuilder::new().build().await?;
/// # let dest_hash = leviculum_core::DestinationHash::new([0; 16]);
/// let endpoint = node.packet_sender(&dest_hash);
///
/// // Send a single packet
/// let _hash = endpoint.send(b"Hello!").await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct PacketSender {
    dest_hash: DestinationHash,
    inner: Arc<Mutex<StdNodeCore>>,
    action_dispatch_tx: mpsc::Sender<TickOutput>,
}

impl PacketSender {
    /// Create a new PacketSender (crate-private, like LinkHandle)
    pub(crate) fn new(
        dest_hash: DestinationHash,
        inner: Arc<Mutex<StdNodeCore>>,
        action_dispatch_tx: mpsc::Sender<TickOutput>,
    ) -> Self {
        Self {
            dest_hash,
            inner,
            action_dispatch_tx,
        }
    }

    /// Get the destination hash for this endpoint
    pub fn dest_hash(&self) -> &DestinationHash {
        &self.dest_hash
    }

    /// Send a single packet to the destination
    ///
    /// Builds an unreliable data packet and queues it for dispatch.
    /// A path to the destination must already be known.
    ///
    /// # Returns
    /// The truncated packet hash, usable for tracking delivery proofs.
    pub async fn send(&self, data: &[u8]) -> Result<[u8; TRUNCATED_HASHBYTES], Error> {
        let (packet_hash, output) = {
            let mut core = self.inner.lock_recover();
            core.send_single_packet(&self.dest_hash, data)?
        };
        self.action_dispatch_tx
            .send(output)
            .await
            .map_err(|_| Error::NotRunning)?;
        Ok(packet_hash)
    }

    /// [`send`](Self::send), additionally reporting the packed wire length of
    /// the frame handed to transport.
    ///
    /// The payload a caller passes in does not predict what goes on the air:
    /// header, ephemeral key, token overhead and block padding are added by
    /// the stack. A caller pricing airtime against an interface's bitrate
    /// needs the wire length, so it asks for it rather than reconstructing
    /// the encryption arithmetic.
    pub async fn send_measured(
        &self,
        data: &[u8],
    ) -> Result<([u8; TRUNCATED_HASHBYTES], usize), Error> {
        let (packet_hash, wire_len, output) = {
            let mut core = self.inner.lock_recover();
            core.send_single_packet_measured(&self.dest_hash, data)?
        };
        self.action_dispatch_tx
            .send(output)
            .await
            .map_err(|_| Error::NotRunning)?;
        Ok((packet_hash, wire_len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::ReticulumNodeBuilder;

    // Returns a TempDir as the third tuple element; callers must bind
    // it to a live variable (e.g. `_td`) so its RAII drop doesn't
    // delete the storage directory out from under the node.
    fn make_node_and_inner() -> (
        Arc<Mutex<StdNodeCore>>,
        mpsc::Sender<TickOutput>,
        tempfile::TempDir,
    ) {
        let td = tempfile::tempdir().expect("tempdir");
        let node = ReticulumNodeBuilder::new()
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync");
        let inner = node.inner();
        let (tx, _rx) = mpsc::channel(16);
        (inner, tx, td)
    }

    #[tokio::test]
    async fn test_packet_sender_dest_hash() {
        let (inner, tx, _td) = make_node_and_inner();
        let dest_hash = DestinationHash::new([0xAB; 16]);
        let ep = PacketSender::new(dest_hash, inner, tx);

        assert_eq!(*ep.dest_hash(), dest_hash);
    }

    #[tokio::test]
    async fn test_packet_sender_send_no_path_returns_error() {
        let (inner, tx, _td) = make_node_and_inner();
        let dest_hash = DestinationHash::new([0xAB; 16]);
        let ep = PacketSender::new(dest_hash, inner, tx);

        let result = ep.send(b"hello").await;
        assert!(result.is_err(), "send with no path should fail");
    }

    #[tokio::test]
    async fn test_packet_sender_send_closed_channel() {
        let (inner, _, _td) = make_node_and_inner();

        // Register a destination and announce so a path exists
        let id = leviculum_core::Identity::generate(&mut rand_core::OsRng);
        let pub_bytes = id.public_key_bytes();
        let dest = leviculum_core::Destination::new(
            Some(id),
            leviculum_core::Direction::In,
            leviculum_core::DestinationType::Single,
            "test",
            &["endpoint"],
        )
        .unwrap();
        let dest_hash = *dest.hash();
        {
            let mut core = inner.lock().unwrap();
            core.register_destination(dest);
            // Teach sender about the identity for encryption
            let pub_identity = leviculum_core::Identity::from_public_key_bytes(&pub_bytes).unwrap();
            core.remember_identity(dest_hash, pub_identity);
        }
        // Announce creates a local path entry
        let _ = inner.lock().unwrap().announce_destination(&dest_hash, None);

        // Create endpoint with a closed channel
        let (tx, rx) = mpsc::channel::<TickOutput>(1);
        drop(rx); // close the receiver

        let ep = PacketSender::new(dest_hash, Arc::clone(&inner), tx);
        let result = ep.send(b"hello").await;
        assert!(result.is_err(), "send on closed channel should fail");
    }
}
