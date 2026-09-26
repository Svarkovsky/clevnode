//! Packet framing for stream-based interfaces.
//!
//! Framing converts variable-length packets into delimited byte sequences so
//! that a receiver on a stream transport (TCP, serial) can find packet
//! boundaries. Datagram transports (UDP, LoRa) deliver discrete packets and
//! do not need framing.
//!
//! # HDLC Framing
//!
//! The [`hdlc`] sub-module implements the framing scheme used by Python
//! Reticulum over TCP: `0x7E` flag delimiters, `0x7D` escape byte with
//! XOR `0x20`, and CRC-16-CCITT for integrity.
//!
//! ```text
//! [FLAG 0x7E] [escaped payload] [CRC-16 hi] [CRC-16 lo] [FLAG 0x7E]
//! ```
//!
//! # KISS Framing
//!
//! The [`kiss`] sub-module implements KISS framing used by serial interfaces
//! (RNode, KISSInterface): `0xC0` (FEND) delimiters, `0xDB` (FESC) escape
//! sequences, and a command byte after the opening delimiter.
//!
//! ```text
//! [FEND 0xC0] [command] [escaped payload] [FEND 0xC0]
//! ```
//!
//! # AX.25 Framing
//!
//! The [`ax25`] sub-module adds an AX.25 UI-frame header (source/destination
//! callsign+SSID, control, PID) on top of KISS, as Python Reticulum's
//! `AX25KISSInterface` does. The header wraps the payload; the whole AX.25 frame
//! is then KISS-framed with [`kiss`].
//!
//! # Usage
//!
//! ```text
//! // Sender: frame a packet
//! let framed = frame(&raw_packet);
//! stream.write_all(&framed);
//!
//! // Receiver: accumulate bytes and extract packets
//! let mut deframer = Deframer::new();
//! deframer.receive(&incoming_bytes);
//! while let Some(packet) = deframer.next_frame() {
//!     process(packet);
//! }
//! ```
//!
//! # Allocation
//!
//! [`frame_to_slice`] and [`crc16`] work without `alloc`.
//! [`Deframer`] and [`frame`] require `alloc`.

pub mod ax25;
pub mod ble;
pub mod hdlc;
pub mod kiss;

// Re-export commonly used items
pub use hdlc::{crc16, frame_to_slice, max_framed_size, needs_escape, ESCAPE, ESCAPE_XOR, FLAG};
pub use hdlc::{frame, frame_with_crc, DeframeResult, Deframer};
