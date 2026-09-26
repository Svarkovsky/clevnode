//! `lxmf-node` — the leviculum counterpart of periculum's `lxmf_node.py`.
//!
//! # What it is for
//!
//! periculum has six LXMF step verbs (`lxmf_start`, `lxmf_announce`,
//! `lxmf_wait_for_peer`, `lxmf_send`, `lxmf_assert_received`, `lxmf_stop`) and
//! until now exactly one thing to point them at: a Python `LXMF.LXMRouter`
//! attached to whichever daemon runs in the container. The daemon under test
//! could be either stack; the *messaging* stack under test could only be
//! Python's.
//!
//! This binary speaks the same protocol — same stdin commands, same `EVENT`
//! line grammar, same exit behaviour — so the same six verbs drive either
//! stack and a scenario becomes a real A/B: ours on one end, Python's on the
//! other, or ours on both, with the driver held constant. That is the
//! `lnsd`/`rnsd` drop-in property applied one layer up, and CLAUDE.md makes it
//! a rule rather than a preference: "the test harness points the same driver
//! at either daemon, never a parallel per-stack driver".
//!
//! [`protocol`] is the contract; [`processor`] is the stack behind it.
//!
//! # What it proves
//!
//! It is the acceptance evidence for Codeberg #196's first criterion — *a
//! std-driver application runs `leviculum-lxmf` end-to-end without forking
//! `leviculum-std`*. That is why it is its own package rather than a `[[bin]]`
//! on either crate: across a package boundary the compiler refuses anything
//! neither crate marks `pub`, so a gap in the public surface shows up as a
//! build error here instead of as a quiet `pub(crate)` widening there.
//!
//! `leviculum-lxmf` was the other candidate and is ruled out by its own
//! manifest: it is `no_std`, and a binary target on it would pull
//! `leviculum-std` and tokio into a `no_std` crate's dependency graph.

pub mod processor;
pub mod protocol;

pub use processor::{
    run_build_worker, BuildJob, Emitter, HelperConfig, Input, LxmfHelperProcessor, Out, Shutdown,
    StampJob,
};
pub use protocol::{parse_command, Command, CommandError};
