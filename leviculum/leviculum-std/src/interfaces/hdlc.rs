//! HDLC framing for stream-based interfaces
//!
//! This module re-exports the HDLC implementation from `leviculum-core::framing::hdlc`.
//! All framing logic lives in core for no_std compatibility.

pub use leviculum_core::framing::hdlc::*;
