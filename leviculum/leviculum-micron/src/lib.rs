//! `leviculum-micron`: a parser for the micron (`.mu`) markup used by NomadNet
//! pages.
//!
//! [`parse`] turns a micron document into a render-agnostic [`MicronDocument`]
//! (a tree of [`Block`]/[`Line`]/[`Span`]). No terminal rendering happens here:
//! colours are kept as [`Color`] (raw nibbles plus resolved RGB) and text
//! attributes as plain flags, so a downstream renderer decides how to draw
//! them.
//!
//! An implementation of the micron markup format: formatting and colour state
//! persists across lines until reset, and malformed input degrades gracefully
//! rather than panicking.
//!
//! ```
//! use leviculum_micron::{parse, Block};
//!
//! let doc = parse(">> Title\n\nSome `!bold`! text.");
//! assert!(matches!(doc.blocks[0], Block::Heading { depth: 2, .. }));
//! ```

mod color;
mod model;
mod parser;

pub use color::Color;
pub use model::{Align, Block, Field, FieldKind, Line, Link, MicronDocument, Span, Style};
pub use parser::parse;
