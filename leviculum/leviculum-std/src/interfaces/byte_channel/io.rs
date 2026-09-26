//! The bidirectional HDLC I/O loop for a byte-channel interface.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use leviculum_core::constants::MTU;
use leviculum_core::framing::hdlc::{frame, DeframeResult, Deframer};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use super::super::{IncomingPacket, InterfaceCounters, OutgoingPacket};
use super::BYTE_CHANNEL_HW_MTU;

/// Frame buffer multiplier (accounts for HDLC escaping overhead).
const FRAME_BUFFER_MULTIPLIER: usize = 2;

/// Read buffer size for pulling bytes off the caller's stream.
const READ_BUF_SIZE: usize = 1024;

/// Await an optional shutdown signal, or pend forever when there is none.
async fn wait_shutdown(shutdown: &mut Option<oneshot::Receiver<()>>) {
    match shutdown.as_mut() {
        Some(rx) => {
            let _ = rx.await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Read path:  stream → HDLC deframe → incoming channel
/// Write path: outgoing channel → HDLC frame → stream → flush
///
/// Enforces `HW_MTU` by handing it to the deframer, which discards a frame
/// growing past the limit — bounding memory on a misbehaving peer (matching
/// the pipe interface).
pub(super) async fn run<R, W>(
    name: String,
    mut read: R,
    mut write: W,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    mut outgoing_rx: mpsc::Receiver<OutgoingPacket>,
    counters: Arc<InterfaceCounters>,
    mut shutdown: Option<oneshot::Receiver<()>>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut deframer = Deframer::with_max_frame(BYTE_CHANNEL_HW_MTU as usize);
    let mut read_buf = vec![0u8; READ_BUF_SIZE];
    let mut frame_buf = Vec::with_capacity(MTU * FRAME_BUFFER_MULTIPLIER);

    loop {
        tokio::select! {
            _ = wait_shutdown(&mut shutdown) => {
                tracing::debug!("Byte-channel interface {}: detached", name);
                return;
            }

            result = read.read(&mut read_buf) => {
                match result {
                    Ok(0) => {
                        tracing::debug!("Byte-channel interface {}: stream EOF", name);
                        return;
                    }
                    Ok(n) => {
                        for r in deframer.process(&read_buf[..n]) {
                            match r {
                                DeframeResult::Frame(data) => {
                                    counters.rx_bytes.fetch_add(data.len() as usize, Ordering::Relaxed);
                                    if incoming_tx.send(IncomingPacket { data }).await.is_err() {
                                        return;
                                    }
                                }
                                // HW_MTU enforcement lives in the deframer now.
                                DeframeResult::Oversized => tracing::trace!(
                                    "Byte-channel {}: frame exceeds HW_MTU, discarded", name
                                ),
                                _ => {}
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!("Byte-channel interface {}: read error: {}", name, e);
                        return;
                    }
                }
            }

            msg = outgoing_rx.recv() => {
                match msg {
                    Some(pkt) => {
                        frame(&pkt.data, &mut frame_buf);
                        if let Err(e) = write.write_all(&frame_buf).await {
                            tracing::debug!("Byte-channel interface {}: write error: {}", name, e);
                            return;
                        }
                        if let Err(e) = write.flush().await {
                            tracing::debug!("Byte-channel interface {}: flush error: {}", name, e);
                            return;
                        }
                        counters.tx_bytes.fetch_add(frame_buf.len() as usize, Ordering::Relaxed);
                    }
                    None => {
                        tracing::debug!("Byte-channel interface {}: outgoing channel closed", name);
                        return;
                    }
                }
            }
        }
    }
}
