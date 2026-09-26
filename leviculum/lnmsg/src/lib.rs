//! `lnmsg` — a terminal LXMF messenger for Reticulum.
//!
//! This crate carries the first slice of the program described in
//! `docs/src/concepts/lnmsg.md`: the non-interactive send path, and nothing
//! more. No TUI, no mailbox, no reading.
//!
//! # The two structural constraints
//!
//! **The LXMF router lives here, never in `lnsd`.** The `lnsd`-resident
//! variant is rejected twice over in the architecture record
//! (`docs/src/concepts/lnmsg-architecture.md` §4): it would put LXMF knowledge
//! into the transport daemon that the Codeberg #196 seam exists to keep clean,
//! and client programs do not merge into `lnsd`.
//!
//! **`lnmsg` attaches to a running shared instance.** It does not bring up a
//! Reticulum stack of its own. With no daemon running, `lnmsg send` fails and
//! says so; it never silently stands up a private stack, because that fallback
//! is a later decision and not part of this slice.
//!
//! # The layering, and why it is drawn where it is
//!
//! The 2026-08-08 process-model decision is "C, built as A first": one process
//! now, with the router and the store behind an interface so that daemon mode
//! is later a wiring change rather than a rewrite. That interface is
//! [`outbox::Outbox`] — commands in, events out, no Reticulum type in the
//! signatures beyond a destination hash and a message id.
//!
//! ```text
//!   main.rs  ──> send::run_send(&mut impl Outbox, …)      the frontend
//!                        │
//!                        │  Command / OutboxEvent          <- the seam
//!                        v
//!   engine::InProcessOutbox ── channels ── engine::Engine   the backend
//!                                            (a CoreProcessor holding an
//!                                             LxmfRouter, under the core lock)
//! ```
//!
//! [`send::run_send`] never names an `LxmfRouter`, a `ReticulumNode` or a
//! channel, which is what makes it testable against a fake outbox with no
//! network anywhere; a daemon-mode `Outbox` speaking to `lnmsgd` over a socket
//! would drop into the same slot.
//!
//! # Honesty about delivery
//!
//! Decided 2026-08-10 (`lnmsg-architecture.md` §6): `send` does not wait for a
//! delivery proof, and its success must never be worded as "delivered" or
//! "sent to" a person. The word this program uses is *queued*, and the one
//! stronger thing it will claim is that the message was *handed on* — that the
//! stack took the bytes and put them on the network. Whether anything received
//! them is a history, not a result, and `lnmsg status` / `--wait` are the
//! (not-yet-built) answers to that question.

pub mod address;
pub mod body;
pub mod display_name;
pub mod engine;
pub mod events;
pub mod identity;
pub mod outbox;
pub mod send;
