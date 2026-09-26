//! The outbound queue entry, with its build epoch enforced at the module
//! boundary.
//!
//! A Resource build captured from an entry commits later, off the caller's
//! lock (Codeberg #196). `commit_resource_build` refuses the build when the
//! entry's `message`, `state` or `propagation` changed in between — those
//! three are what the built bytes and their target were derived from — by
//! comparing the epoch the build captured against the entry's current one.
//!
//! The enforcement is Rust privacy. The three fields are private to this
//! module, and neither `router.rs` (the parent) nor `propagation_runtime.rs`
//! (a sibling) can see private items of a child module, so every mutation in
//! the crate has to go through the methods below — each of which advances the
//! epoch exactly when the value actually changes. No method hands out `&mut`
//! to any of the three fields: an escape hatch like `message_mut()` would be
//! the unenforced hole with a new name. Bookkeeping fields the build does not
//! depend on (`attempts`, `next_attempt_ms`, `progress`) stay public.
//!
//! Two rules the methods implement:
//!
//! * **Value change, not assignment.** The tick's deferral arm re-inserts
//!   `state = Outbound` on an entry that already is `Outbound` every retry
//!   interval. If that advanced the epoch, every build older than one
//!   interval would be refused at commit and a consumer slower than the
//!   interval could never land one.
//! * **Epochs come from one router-level counter.** [`BuildEpochs`] only
//!   counts forward and its `advance` is private, so an epoch cannot be
//!   minted or replayed outside this module. A per-entry counter would
//!   restart at cancel-plus-re-enqueue of identical content (same
//!   content-derived message ID) and validate a pre-cancel build.

use alloc::vec::Vec;

use super::{MessageState, OutboundPropagation};
use crate::{
    constants::STAMP_SIZE,
    message::{Message, MessageError},
};

/// The router-level source of build epochs. Monotonic, never persisted: a
/// restored counter would validate exactly the builds a restore must refuse,
/// and entries restored from a snapshot get fresh values from the live
/// counter instead.
#[derive(Debug, Default)]
pub struct BuildEpochs {
    next: u64,
}

impl BuildEpochs {
    fn advance(&mut self) -> u64 {
        let epoch = self.next;
        self.next = self.next.wrapping_add(1);
        epoch
    }
}

#[derive(Debug, Clone)]
pub struct OutboundEntry {
    message: Message,
    state: MessageState,
    pub attempts: u8,
    pub next_attempt_ms: u64,
    pub progress: f32,
    /// Recipient-encrypted bytes and the independent propagation-node stamp.
    /// This is persisted so a restored queue never re-encrypts a message after
    /// a propagation stamp has already been generated for its transient ID.
    propagation: Option<OutboundPropagation>,
    /// Advanced on every value change of `message`, `state` or `propagation`.
    /// A build captures it and `commit_resource_build` compares; a mismatch is
    /// `RouterError::StaleBuild`.
    build_epoch: u64,
}

impl OutboundEntry {
    pub fn new(
        message: Message,
        state: MessageState,
        attempts: u8,
        next_attempt_ms: u64,
        progress: f32,
        propagation: Option<OutboundPropagation>,
        epochs: &mut BuildEpochs,
    ) -> Self {
        Self {
            message,
            state,
            attempts,
            next_attempt_ms,
            progress,
            propagation,
            build_epoch: epochs.advance(),
        }
    }

    pub fn message(&self) -> &Message {
        &self.message
    }

    pub fn state(&self) -> MessageState {
        self.state
    }

    pub fn propagation(&self) -> Option<&OutboundPropagation> {
        self.propagation.as_ref()
    }

    pub fn build_epoch(&self) -> u64 {
        self.build_epoch
    }

    /// Returns whether the state actually changed. A no-op write does not
    /// advance the epoch.
    pub fn set_state(&mut self, state: MessageState, epochs: &mut BuildEpochs) -> bool {
        if self.state == state {
            return false;
        }
        self.state = state;
        self.build_epoch = epochs.advance();
        true
    }

    /// Attach a delivery stamp to the message (S1). The message ID does not
    /// cover the stamp, so ID and signature are preserved. Re-attaching the
    /// value already present changes nothing and keeps the epoch still; an
    /// invalid stamp errors before anything is written.
    pub fn set_message_stamp(
        &mut self,
        stamp: Vec<u8>,
        epochs: &mut BuildEpochs,
    ) -> Result<(), MessageError> {
        if self.message.stamp.as_deref() == Some(stamp.as_slice()) {
            return Ok(());
        }
        self.message.set_stamp(stamp)?;
        self.build_epoch = epochs.advance();
        Ok(())
    }

    /// Replace the prepared propagation envelope wholesale. Returns whether
    /// the value changed.
    pub fn set_propagation(
        &mut self,
        propagation: Option<OutboundPropagation>,
        epochs: &mut BuildEpochs,
    ) -> bool {
        if self.propagation == propagation {
            return false;
        }
        self.propagation = propagation;
        self.build_epoch = epochs.advance();
        true
    }

    /// Attach the separate propagation-node stamp. Returns whether the value
    /// changed; without a prepared envelope there is nothing to attach and
    /// nothing changes.
    pub fn set_propagation_stamp(
        &mut self,
        stamp: [u8; STAMP_SIZE],
        epochs: &mut BuildEpochs,
    ) -> bool {
        let Some(prepared) = self.propagation.as_mut() else {
            return false;
        };
        if prepared.stamp == Some(stamp) {
            return false;
        }
        prepared.stamp = Some(stamp);
        self.build_epoch = epochs.advance();
        true
    }

    /// Point the prepared envelope at a (possibly absent) node cost: sets
    /// `target_cost` and clears the now-mismatched stamp, iff the cost
    /// actually differs (S3, the node switch). Returns whether anything
    /// changed.
    pub fn retarget_propagation(
        &mut self,
        target_cost: Option<u8>,
        epochs: &mut BuildEpochs,
    ) -> bool {
        let Some(prepared) = self.propagation.as_mut() else {
            return false;
        };
        if prepared.target_cost == target_cost {
            return false;
        }
        prepared.target_cost = target_cost;
        prepared.stamp = None;
        self.build_epoch = epochs.advance();
        true
    }
}
