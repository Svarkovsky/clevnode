use std::sync::Arc;
use std::time::Duration;

use leviculum_core::framing::kiss::{self, KissDeframeResult, KissDeframer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{debug, info};

use crate::rules::{Direction, FrameDecision, KissFrame, RuleEngine};

/// RNode max payload for KISS deframing
const RNODE_MAX_PAYLOAD: usize = 508;

struct DelayedFrame {
    deliver_at: Instant,
    data: Vec<u8>,
    to_a: bool,
}

/// Bidirectional KISS-aware forwarding between two async byte streams.
///
/// Each frame is parsed, evaluated against the rule engine, and then
/// forwarded, dropped, delayed, or corrupted according to the first
/// matching rule.
pub async fn forward_kiss_frames<A, B>(
    mut side_a: A,
    side_a_name: &str,
    mut side_b: B,
    side_b_name: &str,
    engine: Arc<Mutex<RuleEngine>>,
) -> std::io::Result<()>
where
    A: AsyncReadExt + AsyncWriteExt + Unpin,
    B: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let mut deframer_a = KissDeframer::with_max_payload(RNODE_MAX_PAYLOAD);
    let mut deframer_b = KissDeframer::with_max_payload(RNODE_MAX_PAYLOAD);

    let mut buf_a = [0u8; 4096];
    let mut buf_b = [0u8; 4096];
    let mut frame_buf = Vec::with_capacity(1024);

    let mut delayed: Vec<DelayedFrame> = Vec::new();

    loop {
        let next_deadline = delayed.iter().map(|f| f.deliver_at).min();
        let has_delayed = next_deadline.is_some();
        let sleep_target =
            next_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(86400));

        tokio::select! {
            result = side_a.read(&mut buf_a) => {
                let n = result?;
                if n == 0 {
                    info!("{side_a_name} closed");
                    return Ok(());
                }
                let frames = deframer_a.process(&buf_a[..n]);
                for frame in frames {
                    if let KissDeframeResult::Frame { command, ref payload } = frame {
                        let kiss_frame = KissFrame { command, payload: payload.clone() };
                        let decision = engine.lock().await.evaluate(&kiss_frame, Direction::AToB);
                        let decision_tag = match &decision {
                            FrameDecision::Forward => "FWD",
                            FrameDecision::Drop => "DROP",
                            FrameDecision::Delay(_) => "DELAY",
                            FrameDecision::Corrupt(_) => "CORRUPT",
                        };
                        debug!("AToB cmd=0x{command:02X} payload_len={} {decision_tag}", payload.len());
                        apply_decision(
                            decision, command, payload, &mut frame_buf,
                            &mut side_b, &mut delayed, false,
                            side_a_name, side_b_name,
                        ).await?;
                    }
                }
            }

            result = side_b.read(&mut buf_b) => {
                let n = result?;
                if n == 0 {
                    info!("{side_b_name} closed");
                    return Ok(());
                }

                let frames = deframer_b.process(&buf_b[..n]);
                for frame in frames {
                    if let KissDeframeResult::Frame { command, ref payload } = frame {
                        debug!("BToA cmd=0x{command:02X} payload_len={}", payload.len());
                        let kiss_frame = KissFrame { command, payload: payload.clone() };
                        let decision = engine.lock().await.evaluate(&kiss_frame, Direction::BToA);
                        apply_decision(
                            decision, command, payload, &mut frame_buf,
                            &mut side_a, &mut delayed, true,
                            side_b_name, side_a_name,
                        ).await?;
                    }
                }
            }

            _ = tokio::time::sleep_until(sleep_target), if has_delayed => {
                let now = Instant::now();
                let mut i = 0;
                while i < delayed.len() {
                    if delayed[i].deliver_at <= now {
                        let df = delayed.remove(i);
                        if df.to_a {
                            side_a.write_all(&df.data).await?;
                        } else {
                            side_b.write_all(&df.data).await?;
                        }
                        debug!("Delivered delayed frame");
                    } else {
                        i += 1;
                    }
                }
            }
        }
    }
}

/// Apply a frame decision: forward, drop, delay, or corrupt.
#[allow(clippy::too_many_arguments)]
async fn apply_decision<W: AsyncWriteExt + Unpin>(
    decision: FrameDecision,
    command: u8,
    payload: &[u8],
    frame_buf: &mut Vec<u8>,
    target: &mut W,
    delayed: &mut Vec<DelayedFrame>,
    to_a: bool,
    src_name: &str,
    dst_name: &str,
) -> std::io::Result<()> {
    match decision {
        FrameDecision::Forward => {
            debug!(
                direction = %format!("{src_name} -> {dst_name}"),
                cmd = format!("0x{command:02X}"),
                len = payload.len(),
                "KISS frame"
            );
            kiss::frame(command, payload, frame_buf);
            target.write_all(frame_buf).await?;
        }
        FrameDecision::Drop => {
            debug!(
                direction = %format!("{src_name} -> {dst_name}"),
                cmd = format!("0x{command:02X}"),
                "KISS frame DROPPED"
            );
        }
        FrameDecision::Delay(ms) => {
            debug!(
                direction = %format!("{src_name} -> {dst_name}"),
                cmd = format!("0x{command:02X}"),
                delay_ms = ms,
                "KISS frame DELAYED"
            );
            kiss::frame(command, payload, frame_buf);
            delayed.push(DelayedFrame {
                deliver_at: Instant::now() + Duration::from_millis(ms),
                data: frame_buf.clone(),
                to_a,
            });
        }
        FrameDecision::Corrupt(ref corrupted_payload) => {
            debug!(
                direction = %format!("{src_name} -> {dst_name}"),
                cmd = format!("0x{command:02X}"),
                "KISS frame CORRUPTED"
            );
            kiss::frame(command, corrupted_payload, frame_buf);
            target.write_all(frame_buf).await?;
        }
    }
    Ok(())
}
