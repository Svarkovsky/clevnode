//! `lnflash` — bring an LNode board up on our firmware from a self-contained
//! bundle: one static binary, the images beside it, no repo, no network, no
//! external programs.
//!
//! The design this implements is `docs/src/concepts/lnode-flashing.md`. Two
//! things from it shape every module here.
//!
//! **A board is four independent answers, not one unit.** Identify, enter,
//! transport, verify — with preconditions crossing all four. Each is its own
//! module, so a second board is data entry and a second chip family costs
//! exactly one new transport. Nothing board-specific belongs in code: board
//! facts live in the manifest, and an `if board == "t114"` anywhere outside
//! [`manifest`] would mean the split is wrong.
//!
//! **The bootloader is the board's identity, never the running firmware.**
//! An application's USB ID belongs to whatever happens to be installed; the
//! `Board-ID` in `INFO_UF2.TXT` belongs to the board. So identity is
//! established in two stages — candidates first, the real answer only after
//! entering the bootloader — and no write may rest on the first stage.
//! Commit `362c1c2d` records a T114 image landing on a RAK4631 when that
//! rule was absent.

pub mod entry;
pub mod envelope;
pub mod flow;
pub mod ihex;
pub mod infouf2;
pub mod manifest;
pub mod media;
pub mod name;
pub mod position;
pub mod radio;
pub mod softdevice;
pub mod summarize;
pub mod sys;
pub mod telemetry;
pub mod transport;
pub mod uf2;
pub mod ui;
pub mod usb;
pub mod verify;
pub mod watch;

// The committed sysfs fixture tree encodes ':' as '+' (NTFS cannot check a
// colon out); this helper materializes the decoded tree for unit tests.
#[cfg(test)]
#[path = "../tests/fixtures/materialize.rs"]
pub(crate) mod sysfs_fixture;
