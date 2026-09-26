//! LXMF wire format, delivery, and anti-spam primitives.
//!
//! This crate is `no_std + alloc` and contains no platform I/O. Its transport
//! adapters drive `leviculum-core` and return actions for the application to
//! dispatch.
//!
//! # Propagation
//!
//! Propagation support covers both ends of the *client ↔ node* exchange.
//! [`PropagationUpload`] encodes the Python-compatible origin upload envelope
//! and decodes it again on the receiving node, [`MessageGetRequest`] and the
//! [`MessageListResponse`] / [`MessageGetResponse`] pair carry the `/get`
//! list, download, and acknowledgement exchange in both directions,
//! [`PropagationSignal`] carries a node's refusal of an upload, and
//! [`PropagationNodeAnnounce`] both encodes a node's own discovery data and
//! decodes another node's. A host built on these codecs still owns its own
//! mailbox storage, stamp validation, links and resources: this crate
//! performs no I/O.
//!
//! What the crate does **not** implement is the *node ↔ node* direction. A
//! Python propagation node syncs with its peers over a second endpoint,
//! `/offer`, whose upload payload is the same wire shape carrying more than
//! one message, admitted only against a validated peering key
//! (`LXMRouter.propagation_resource_concluded`,
//! reference/LXMF/LXMF/LXMRouter.py:2336-2345, and the peering-key branch at
//! :2377-2385). That is why [`PropagationUpload::decode`] accepts the
//! singleton envelope only and answers the multi-message form with
//! [`PropagationError::MultipleMessages`]: it is a message for an endpoint
//! this crate does not serve, not a malformed one. Peer sync, peering keys
//! and the `/offer` path are leviculum#209.

#![no_std]
#[cfg(test)]
extern crate std;

extern crate alloc;

pub mod announce;
pub mod attachments;
pub mod constants;
pub mod message;
pub mod msgpack;
pub mod node;
pub mod paper;
pub mod propagation;
pub mod propagation_client;
pub mod router;
pub mod stamp;
pub mod storage;
pub mod telemetry;
pub mod ticket;

pub use attachments::{
    AttachmentError, AudioAttachment, FileAttachment, ImageAttachment, MessageAttachments,
};
pub use message::{DeliveryMethod, Field, Message, MessageError, Verification};
pub use node::{
    DeliveryFailure, DeliveryRepresentation, DirectLinkState, InboundRejection,
    IncomingResourceTransfer, LxmfNode, LxmfNodeConfig, LxmfNodeError, LxmfNodeEvent,
    LxmfNodeOutput, LxmfResourceSendParams, PreparedLxmfSend, SubmissionId,
};
pub use paper::{PaperError, PaperMessage};
pub use propagation::{
    MessageGetRequest, MessageGetResponse, MessageListResponse, MetadataEntry, PeerError,
    PropagatedMessage, PropagationError, PropagationNodeAnnounce, PropagationSignal,
    PropagationUpload, TransferLimit, TransientId, MESSAGE_GET_PATH,
};
pub use propagation_client::{
    KnownPropagationNode, PreparedUpload, PropagationRequestKind, PropagationTransport,
    PropagationTransportError, PropagationTransportEvent, PropagationTransportOutput,
    PropagationUploadFailure, PropagationUploadRepresentation, UploadSendParams,
    PROPAGATION_ASPECT,
};
pub use router::{
    BuiltResource, DeliveryStampRequest, InboundStampRequest, PendingResourceBuild,
    PropagationStampRequest,
};
#[cfg(feature = "pow")]
pub use stamp::{CooperativeStamper, CooperativeYield, StampError, StampExecutor, Yield};
