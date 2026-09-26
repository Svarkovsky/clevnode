//! The seam between the frontend and the message engine.
//!
//! Decision 1 of the design record (`docs/src/concepts/lnmsg.md`, 2026-08-08)
//! is "C, built as A first": one process now, **with the router and the store
//! behind an interface** so that daemon mode later is a wiring change and not
//! a rewrite. This module is that interface.
//!
//! It is deliberately a command/event seam rather than a set of methods that
//! return answers. Two reasons, and they are the same two the architecture
//! record gives for the shape of the whole program:
//!
//! * The engine runs inside the driver's tick, under a non-reentrant core
//!   mutex, and everything crossing that boundary is already a non-blocking
//!   queue push (`docs/src/concepts/lnmsg-architecture.md` §3). A seam shaped
//!   like the traffic that actually crosses it needs no adapter.
//! * A daemon-mode implementation is a socket, and a socket is a command
//!   stream and an event stream. An interface built from request/response
//!   methods would have to be rewritten into this shape the day `lnmsgd`
//!   exists, which is exactly what the decision forbids.
//!
//! Nothing here mentions `LxmfRouter`, `ReticulumNode` or a channel type, so
//! [`crate::send`] can be driven by a fake with no network anywhere.

use leviculum_lxmf::router::MessageState;

/// How a message should reach its destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// Straight to the peer over the mesh.
    Direct,
    /// Through a propagation node's mailbox. Not built in this slice.
    Propagated,
}

impl Via {
    /// The word the event log and `--via` use.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Propagated => "propagated",
        }
    }
}

/// One message, as the frontend describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendRequest {
    pub destination: [u8; 16],
    pub title: Vec<u8>,
    pub body: Vec<u8>,
    pub via: Via,
}

/// What the frontend asks the engine to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Learn the destination's identity and a path to it. A message cannot be
    /// encrypted to a peer whose public key we have never seen, so this is a
    /// precondition and not an optimisation.
    Resolve { destination: [u8; 16] },
    /// Queue one message.
    Send(Box<SendRequest>),
}

/// What the engine reports back.
///
/// Ordered by when they can first appear, which is also the order
/// [`crate::send::run_send`] waits for them in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboxEvent {
    /// The engine registered its delivery destination. `address` is ours.
    Ready { address: [u8; 16] },
    /// The engine could not become ready at all, and never will.
    Broken { detail: String },
    /// A [`Command::Resolve`] destination is now reachable.
    Resolved { destination: [u8; 16] },
    /// The router accepted a message and gave it an id.
    Queued { message_id: [u8; 32] },
    /// The router refused a message. No id exists.
    Refused { detail: String },
    /// One outbound state transition.
    State {
        message_id: [u8; 32],
        state: MessageState,
    },
    /// The message left the router's outbound queue, which the router does
    /// only in a terminal state (`leviculum-lxmf/src/router.rs:771-773`).
    /// `last` is the last state seen for it, when there was one.
    Left {
        message_id: [u8; 32],
        last: Option<MessageState>,
    },
}

/// The engine went away — in this slice, the driver detached a panicking
/// processor, or the node was torn down under us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxGone;

impl std::fmt::Display for OutboxGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the message engine stopped answering")
    }
}

impl std::error::Error for OutboxGone {}

/// The engine, as a frontend sees it.
///
/// `submit` must not block: in the in-process implementation the far end is a
/// hook running under the core mutex, and a frontend that could stall there
/// would stall the whole node. `try_next_event` must not block either, for the
/// same reason from the other side — the caller decides how long to wait and
/// how to spend the time.
pub trait Outbox {
    /// Hand one command to the engine. Non-blocking.
    fn submit(&self, command: Command) -> Result<(), OutboxGone>;

    /// Take the next event if one is waiting. Never blocks.
    fn try_next_event(&mut self) -> Result<Option<OutboxEvent>, OutboxGone>;
}

#[cfg(test)]
pub(crate) mod fake {
    //! A scripted [`Outbox`] for the frontend's own tests.

    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;

    /// Answers commands from a script, records what it was asked to do.
    pub struct FakeOutbox {
        /// Events handed out in order, one per `try_next_event` call.
        pub events: VecDeque<OutboxEvent>,
        /// Every command the frontend submitted, in order.
        pub commands: RefCell<Vec<Command>>,
        /// When set, every call fails as though the engine had died.
        pub gone: bool,
    }

    impl FakeOutbox {
        pub fn new(events: Vec<OutboxEvent>) -> Self {
            Self {
                events: events.into(),
                commands: RefCell::new(Vec::new()),
                gone: false,
            }
        }
    }

    impl Outbox for FakeOutbox {
        fn submit(&self, command: Command) -> Result<(), OutboxGone> {
            if self.gone {
                return Err(OutboxGone);
            }
            self.commands.borrow_mut().push(command);
            Ok(())
        }

        fn try_next_event(&mut self) -> Result<Option<OutboxEvent>, OutboxGone> {
            if self.gone {
                return Err(OutboxGone);
            }
            Ok(self.events.pop_front())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn via_spells_itself_the_way_the_flag_does() {
        assert_eq!(Via::Direct.as_str(), "direct");
        assert_eq!(Via::Propagated.as_str(), "propagated");
    }
}
