//! Detached cooperative stamp work for the router.
//!
//! Stamp requests contain all cryptographic work coordinates. Their futures
//! borrow only the selected [`StampExecutor`], never [`LxmfRouter`] or
//! `NodeCore`, so the application can continue feeding receive and timer
//! events to the protocol state machines while a single-threaded worker yields.

use super::{
    DeliveryStampRequest, InboundStampRequest, LxmfRouter, PropagationStampRequest, RouterError,
    RouterEvent, RouterOutput,
};

#[cfg(feature = "pow")]
use crate::{
    constants::{WORKBLOCK_EXPAND_ROUNDS, WORKBLOCK_EXPAND_ROUNDS_PN},
    stamp::{StampError, StampExecutor},
};

#[cfg(feature = "pow")]
impl DeliveryStampRequest {
    /// Generate this recipient delivery stamp with a detached executor.
    ///
    /// Pass [`crate::CooperativeStamper::cooperative`] for the default
    /// single-threaded yielding behavior, or any custom [`StampExecutor`] for
    /// a worker pool, Rayon adapter, or hardware implementation.
    pub async fn generate_with<E: StampExecutor>(
        &self,
        executor: &mut E,
    ) -> Result<[u8; 32], StampError> {
        executor
            .generate(&self.message_id, self.target_cost, WORKBLOCK_EXPAND_ROUNDS)
            .await
    }
}

#[cfg(feature = "pow")]
impl PropagationStampRequest {
    /// Generate the independent propagation-node stamp over the transient ID.
    pub async fn generate_with<E: StampExecutor>(
        &self,
        executor: &mut E,
    ) -> Result<[u8; 32], StampError> {
        executor
            .generate(
                &self.transient_id,
                self.target_cost,
                WORKBLOCK_EXPAND_ROUNDS_PN,
            )
            .await
    }
}

#[cfg(feature = "pow")]
impl InboundStampRequest {
    /// Validate this inbound stamp without borrowing the router that retains
    /// the corresponding message.
    pub async fn validate_with<E: StampExecutor>(
        &self,
        executor: &mut E,
    ) -> Result<bool, StampError> {
        Ok(executor
            .validate(
                &self.message_id,
                &self.stamp,
                self.target_cost,
                WORKBLOCK_EXPAND_ROUNDS,
            )
            .await?
            .is_some())
    }
}

impl LxmfRouter {
    /// Apply a detached inbound validation result to the still-queued message.
    ///
    /// The request must still match the message, stamp and configured cost;
    /// stale or duplicate results are rejected without changing router state.
    pub fn set_inbound_stamp_result(
        &mut self,
        request: &InboundStampRequest,
        valid: bool,
    ) -> Result<RouterOutput, RouterError> {
        if self.config.inbound_stamp_cost.unwrap_or(0) != request.target_cost {
            return Err(RouterError::StaleStampRequest);
        }
        let position = self
            .pending_inbound_stamps
            .iter()
            .position(|(message, _)| {
                message.message_id == request.message_id
                    && message.stamp.as_deref() == Some(request.stamp.as_slice())
            })
            .ok_or(RouterError::StaleStampRequest)?;
        let (message, now_unix) = self
            .pending_inbound_stamps
            .remove(position)
            .ok_or(RouterError::StaleStampRequest)?;

        let mut output = RouterOutput::default();
        if valid {
            self.accept_inbound(message, now_unix, &mut output.events);
        } else {
            output
                .events
                .push(RouterEvent::InvalidStamp(request.message_id));
        }
        Ok(self.finish_output(output))
    }
}
