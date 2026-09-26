//! Paper-message encoding and ingestion.

use alloc::{vec, vec::Vec};

use leviculum_core::{crypto::full_hash, Clock, DestinationHash, NodeCore, Storage, TickOutput};
use rand_core::CryptoRngCore;

use super::{unpack_local, DeliveryMethod, LxmfRouter, RouterError, RouterEvent, RouterOutput};
use crate::{
    constants::PAPER_MDU,
    node::LxmfNodeError,
    paper::{PaperError, PaperMessage},
    Message,
};

impl LxmfRouter {
    pub fn ingest_paper<R, C, S>(
        &mut self,
        node: &NodeCore<R, C, S>,
        uri: &str,
    ) -> Result<RouterOutput, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let now_unix = super::emission_secs(node);
        let paper = PaperMessage::from_uri(uri)?;
        if paper.destination_hash() != self.node.delivery_destination_hash().as_bytes() {
            return Err(PaperError::WrongDestination.into());
        }
        let transient_id = full_hash(&paper.to_bytes());
        if self.processed_ids.contains_key(&transient_id) {
            return Ok(self.finish_output(RouterOutput {
                core: TickOutput::default(),
                events: vec![RouterEvent::Duplicate(transient_id)],
            }));
        }
        let destination = node
            .destination(&self.node.delivery_destination_hash())
            .ok_or(RouterError::NotFound)?;
        let packed = paper.decrypt(destination)?;
        let message = unpack_local(node, &packed, DeliveryMethod::Paper)?;
        // Commit the durable de-duplication state only after all fallible
        // parsing and decryption has succeeded, so an error cannot silently
        // mutate the checkpoint without returning an output event.
        self.insert_bounded_id(transient_id, now_unix, false);
        self.insert_bounded_id(transient_id, now_unix, true);
        let mut events = Vec::new();
        // Paper messages intentionally bypass stamp enforcement, matching Python.
        self.accept_inbound(message, now_unix, &mut events);
        Ok(self.finish_output(RouterOutput {
            core: TickOutput::default(),
            events,
        }))
    }

    pub fn paper_message<R, C, S>(
        &self,
        node: &mut NodeCore<R, C, S>,
        message: &Message,
    ) -> Result<PaperMessage, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let packed = message.pack();
        let encrypted = node
            .encrypt_for_destination(
                &DestinationHash::new(message.destination_hash),
                &packed[16..],
            )
            .map_err(LxmfNodeError::Send)?;
        let mut bytes = Vec::with_capacity(16 + encrypted.len());
        bytes.extend_from_slice(&message.destination_hash);
        bytes.extend_from_slice(&encrypted);
        if bytes.len() > PAPER_MDU {
            return Err(PaperError::TooLarge.into());
        }
        Ok(PaperMessage::from_bytes(&bytes)?)
    }
}
