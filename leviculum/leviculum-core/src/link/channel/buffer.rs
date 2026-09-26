//! Buffer system for binary stream transfer over channels (no_std compatible)
//!
//! This module provides buffered I/O primitives for transferring binary data
//! over Reticulum channels using StreamDataMessage.
//!
//! # Overview
//!
//! - `RawChannelReader` - Receives StreamDataMessage and buffers data for reading
//! - `RawChannelWriter` - Prepares data chunks as StreamDataMessage for sending
//! - `BufferedChannelWriter` - Accumulates data before chunking into messages
//!
//! # Wire Compatibility
//!
//! The wire format is fully compatible with Python Reticulum's Buffer system.
//! StreamDataMessage uses MSGTYPE 0xff00 and a 2-byte header encoding:
//! - bits 0-13: stream_id (0-16383)
//! - bit 14: compressed flag
//! - bit 15: EOF flag
//!
//! # Compression
//!
//! When the `compression` feature is enabled, this module provides BZ2 compression
//! support via the `compression` submodule. This enables:
//!
//! - `RawChannelReader::receive_decompress()` - Automatically decompress received messages
//! - `CompressingWriter` - A writer that applies BZ2 compression to outgoing data
//!
//! Without the `compression` feature, data is sent uncompressed (compressed=false).
//! Both modes are fully wire-compatible with Python Reticulum.
//!
//! # Example
//!
//! ```ignore
//! use leviculum_core::link::channel::{RawChannelReader, RawChannelWriter, StreamDataMessage};
//!
//! // Create writer for stream 0
//! let writer = RawChannelWriter::new(0, 458); // channel MDU
//!
//! // Prepare a chunk to send
//! let (msg, consumed) = writer.prepare_chunk(b"Hello, world!", false);
//! // Send msg via channel.send_system(...)
//!
//! // On receiver side
//! let mut reader = RawChannelReader::new(0);
//! reader.receive(&msg);
//!
//! // Read data
//! let mut buf = [0u8; 64];
//! let n = reader.read(&mut buf);
//! ```

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use super::StreamDataMessage;
use crate::constants::{CHANNEL_ENVELOPE_HEADER_SIZE, STREAM_DATA_HEADER_SIZE};

/// Maximum chunk length for a single write operation (16 KB)
pub const MAX_CHUNK_LEN: usize = 16 * 1024;

/// Calculate the maximum data length for a StreamDataMessage given the channel MDU
#[inline]
pub fn max_data_len(channel_mdu: usize) -> usize {
    channel_mdu.saturating_sub(STREAM_DATA_HEADER_SIZE)
}

/// Calculate the total overhead for StreamDataMessage (stream header + envelope header)
#[inline]
pub const fn stream_overhead() -> usize {
    STREAM_DATA_HEADER_SIZE + CHANNEL_ENVELOPE_HEADER_SIZE
}

/// Result of a read operation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadResult {
    /// Successfully read N bytes
    Read(usize),
    /// No data available, would block (not EOF)
    WouldBlock,
    /// End of stream reached, no more data
    Eof,
}

/// Reader for receiving binary stream data over a channel
///
/// `RawChannelReader` buffers incoming `StreamDataMessage` data and provides
/// a read interface for consuming the buffered data.
///
/// # no_std Compatibility
///
/// This type uses `alloc::collections::VecDeque` for buffering and works
/// in no_std environments with the alloc crate.
///
/// # Compression
///
/// This reader does NOT handle decompression. If you receive compressed
/// messages (StreamDataMessage.compressed == true), you must decompress
/// the data before calling `receive()`, or use the compression wrappers
/// in `leviculum-std`.
#[derive(Debug)]
#[allow(dead_code)] // Buffer API not yet integrated — see Codeberg issue #21
pub(crate) struct RawChannelReader {
    /// Stream identifier this reader accepts
    stream_id: u16,
    /// Internal buffer for received data
    buffer: VecDeque<u8>,
    /// Whether EOF has been received
    eof: bool,
}

#[allow(dead_code)] // Buffer API not yet integrated — see Codeberg issue #21
impl RawChannelReader {
    /// Create a new reader for the given stream ID
    ///
    /// # Arguments
    /// * `stream_id` - The stream identifier to accept messages for (0-16383)
    pub(crate) fn new(stream_id: u16) -> Self {
        Self {
            stream_id,
            buffer: VecDeque::new(),
            eof: false,
        }
    }

    /// Get the stream ID this reader is listening on
    pub(crate) fn stream_id(&self) -> u16 {
        self.stream_id
    }

    /// Check if EOF has been received
    pub(crate) fn is_eof(&self) -> bool {
        self.eof
    }

    /// Get the number of bytes available in the buffer
    pub(crate) fn available(&self) -> usize {
        self.buffer.len()
    }

    /// Check if data is available to read
    pub(crate) fn has_data(&self) -> bool {
        !self.buffer.is_empty()
    }

    /// Receive a StreamDataMessage
    ///
    /// Processes the message and buffers its data if it matches this reader's stream_id.
    /// Returns true if the message was accepted (stream_id matched).
    ///
    /// # Arguments
    /// * `message` - The StreamDataMessage to process
    ///
    /// # Compression Note
    ///
    /// If the message has `compressed == true`, the caller must decompress
    /// `message.data` before calling this method. This no_std implementation
    /// does not include decompression.
    pub(crate) fn receive(&mut self, message: &StreamDataMessage) -> bool {
        if message.stream_id != self.stream_id {
            return false;
        }

        self.buffer.extend(&message.data);

        if message.eof {
            self.eof = true;
        }

        true
    }

    /// Receive raw data directly (for pre-decompressed data)
    ///
    /// Use this when you've already decompressed the data externally.
    ///
    /// # Arguments
    /// * `data` - The data to buffer
    /// * `eof` - Whether this marks end of stream
    pub(crate) fn receive_data(&mut self, data: &[u8], eof: bool) {
        self.buffer.extend(data);
        if eof {
            self.eof = true;
        }
    }

    /// Read data from the buffer
    ///
    /// # Arguments
    /// * `buf` - Buffer to read into
    ///
    /// # Returns
    /// - `ReadResult::Read(n)` - Successfully read n bytes
    /// - `ReadResult::WouldBlock` - No data available (not EOF)
    /// - `ReadResult::Eof` - End of stream, no more data will arrive
    pub(crate) fn read(&mut self, buf: &mut [u8]) -> ReadResult {
        if self.buffer.is_empty() {
            if self.eof {
                return ReadResult::Eof;
            }
            return ReadResult::WouldBlock;
        }

        let len = buf.len().min(self.buffer.len());
        for (i, byte) in self.buffer.drain(..len).enumerate() {
            buf[i] = byte;
        }

        ReadResult::Read(len)
    }

    /// Read all available data into a new Vec
    ///
    /// Returns the buffered data and clears the internal buffer.
    pub(crate) fn read_all(&mut self) -> Vec<u8> {
        self.buffer.drain(..).collect()
    }

    /// Peek at available data without consuming it
    ///
    /// # Arguments
    /// * `buf` - Buffer to copy data into
    ///
    /// # Returns
    /// Number of bytes copied (may be less than buf.len())
    pub(crate) fn peek(&self, buf: &mut [u8]) -> usize {
        let len = buf.len().min(self.buffer.len());
        for (i, &byte) in self.buffer.iter().take(len).enumerate() {
            buf[i] = byte;
        }
        len
    }

    /// Clear buffered data without resetting EOF state
    ///
    /// Use this to discard unread data while preserving the EOF flag.
    /// For example, if you want to ignore remaining data after an error
    /// but still respect that the stream has ended.
    ///
    /// To fully reset the reader (clear buffer AND reset EOF), use [`reset`](Self::reset).
    pub(crate) fn clear_buffer(&mut self) {
        self.buffer.clear();
    }

    /// Reset the reader to initial state
    ///
    /// Clears all buffered data AND resets the EOF flag, allowing the reader
    /// to accept new data for a new stream on the same stream_id.
    pub(crate) fn reset(&mut self) {
        self.buffer.clear();
        self.eof = false;
    }
}

/// Writer for sending binary stream data over a channel
///
/// `RawChannelWriter` prepares data chunks as `StreamDataMessage` for transmission.
/// It handles chunking data to fit within the channel MDU.
///
/// # no_std Compatibility
///
/// This type is fully no_std compatible. It does not perform compression.
/// For compression support, use the wrappers in `leviculum-std`.
///
/// # Usage Pattern
///
/// ```ignore
/// let writer = RawChannelWriter::new(stream_id, channel_mdu);
///
/// // Prepare chunk (no compression)
/// let (msg, consumed) = writer.prepare_chunk(data, false);
///
/// // Send via channel
/// channel.send_system(&msg, link_mdu, now_ms, rtt_ms)?;
///
/// // When done, send EOF
/// let msg = writer.prepare_eof();
/// channel.send_system(&msg, link_mdu, now_ms, rtt_ms)?;
/// ```
#[derive(Debug, Clone)]
pub(crate) struct RawChannelWriter {
    /// Stream identifier for outgoing messages
    stream_id: u16,
    /// Maximum data length per message
    max_data_len: usize,
}

impl RawChannelWriter {
    /// Create a new writer for the given stream ID
    ///
    /// # Arguments
    /// * `stream_id` - The stream identifier to send messages as (0-16383)
    /// * `channel_mdu` - The channel's maximum data unit (from `Channel::mdu()`)
    pub(crate) fn new(stream_id: u16, channel_mdu: usize) -> Self {
        Self {
            stream_id,
            max_data_len: max_data_len(channel_mdu),
        }
    }

    /// Get the stream ID this writer sends as
    pub(crate) fn stream_id(&self) -> u16 {
        self.stream_id
    }

    /// Get the maximum data length per chunk
    pub(crate) fn max_data_len(&self) -> usize {
        self.max_data_len
    }

    /// Prepare data for transmission as a StreamDataMessage
    ///
    /// This method chunks data to fit within max_data_len. No compression
    /// is applied (compressed flag will be false).
    ///
    /// # Arguments
    /// * `data` - The data to send
    /// * `eof` - Whether this is the last chunk (sets EOF flag)
    ///
    /// # Returns
    /// A tuple of (StreamDataMessage, bytes_consumed)
    pub(crate) fn prepare_chunk(&self, data: &[u8], eof: bool) -> (StreamDataMessage, usize) {
        let chunk_len = data.len().min(MAX_CHUNK_LEN).min(self.max_data_len);
        let msg = StreamDataMessage::new(
            self.stream_id,
            data[..chunk_len].to_vec(),
            eof,
            false, // No compression in no_std
        );
        (msg, chunk_len)
    }

    /// Prepare a pre-compressed chunk for transmission
    ///
    /// Use this when you've compressed the data externally and want to
    /// send it with the compressed flag set.
    ///
    /// # Arguments
    /// * `compressed_data` - The compressed data
    /// * `eof` - Whether this is the last chunk
    ///
    /// # Returns
    /// The StreamDataMessage ready to send
    #[cfg(feature = "compression")]
    pub(crate) fn prepare_compressed_chunk(
        &self,
        compressed_data: Vec<u8>,
        eof: bool,
    ) -> StreamDataMessage {
        StreamDataMessage::new(self.stream_id, compressed_data, eof, true)
    }

    /// Prepare an EOF marker message
    ///
    /// This creates an empty message with the EOF flag set.
    pub(crate) fn prepare_eof(&self) -> StreamDataMessage {
        StreamDataMessage::eof(self.stream_id)
    }
}

/// Buffered writer that accumulates data before sending
///
/// This writer accumulates data internally and produces StreamDataMessage
/// chunks when `take_pending()` is called.
///
/// # no_std Compatibility
///
/// This type is fully no_std compatible. Messages are produced without
/// compression. For compression support, use the wrappers in `leviculum-std`.
#[derive(Debug)]
#[allow(dead_code)] // Buffer API not yet integrated — see Codeberg issue #21
pub(crate) struct BufferedChannelWriter {
    /// The underlying raw writer
    raw: RawChannelWriter,
    /// Buffered data waiting to be sent
    buffer: Vec<u8>,
    /// Whether EOF has been signaled
    eof: bool,
}

#[allow(dead_code)] // Buffer API not yet integrated — see Codeberg issue #21
impl BufferedChannelWriter {
    /// Create a new buffered writer
    ///
    /// # Arguments
    /// * `stream_id` - The stream identifier to send messages as (0-16383)
    /// * `channel_mdu` - The channel's maximum data unit
    pub(crate) fn new(stream_id: u16, channel_mdu: usize) -> Self {
        Self {
            raw: RawChannelWriter::new(stream_id, channel_mdu),
            buffer: Vec::new(),
            eof: false,
        }
    }

    /// Get the stream ID
    pub(crate) fn stream_id(&self) -> u16 {
        self.raw.stream_id()
    }

    /// Get the maximum data length per chunk
    pub(crate) fn max_data_len(&self) -> usize {
        self.raw.max_data_len()
    }

    /// Get the number of bytes buffered
    pub(crate) fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// Check if there is pending data
    pub(crate) fn has_pending(&self) -> bool {
        !self.buffer.is_empty() || self.eof
    }

    /// Write data to the buffer
    ///
    /// # Arguments
    /// * `data` - Data to buffer
    ///
    /// # Returns
    /// Number of bytes written (always data.len())
    pub(crate) fn write(&mut self, data: &[u8]) -> usize {
        self.buffer.extend_from_slice(data);
        data.len()
    }

    /// Mark the stream as finished (will set EOF on next take_pending)
    pub(crate) fn finish(&mut self) {
        self.eof = true;
    }

    /// Check if finish() has been called
    pub(crate) fn is_finished(&self) -> bool {
        self.eof
    }

    /// Take all pending data as StreamDataMessage chunks
    ///
    /// Returns a vector of messages ready to send. If `finish()` was called,
    /// the last message will have EOF set.
    ///
    /// Messages are produced without compression.
    pub(crate) fn take_pending(&mut self) -> Vec<StreamDataMessage> {
        let mut messages = Vec::new();

        while !self.buffer.is_empty() {
            let is_last = self.eof && self.buffer.len() <= self.raw.max_data_len();
            let (msg, consumed) = self.raw.prepare_chunk(&self.buffer, is_last);
            self.buffer.drain(..consumed);
            messages.push(msg);
        }

        // If we're at EOF and buffer is empty, send EOF marker
        if self.eof && messages.is_empty() {
            messages.push(self.raw.prepare_eof());
        }

        // Clear EOF flag after sending
        if self.eof && !messages.is_empty() {
            self.eof = false;
        }

        messages
    }

    /// Clear buffered data without sending
    pub(crate) fn clear(&mut self) {
        self.buffer.clear();
        self.eof = false;
    }

    /// Get access to the underlying RawChannelWriter
    pub(crate) fn raw_writer(&self) -> &RawChannelWriter {
        &self.raw
    }
}

// Compression Support
#[cfg(feature = "compression")]
mod compression_support {
    //! BZ2 compression extensions for the buffer system (no_std compatible)

    use super::*;
    use crate::compression::{compress, decompress_auto, CompressionError};

    /// Minimum data size to attempt compression (smaller data often doesn't compress well)
    pub const COMPRESSION_MIN_SIZE: usize = 32;

    /// Number of compression attempts with decreasing chunk sizes
    pub const COMPRESSION_TRIES: usize = 4;

    /// Maximum decompressed size for safety (1 MB)
    pub const MAX_DECOMPRESS_SIZE: usize = 1024 * 1024;

    #[allow(dead_code)] // Buffer API not yet integrated — see Codeberg issue #21
    impl RawChannelReader {
        /// Receive and decompress a StreamDataMessage
        ///
        /// If the message has the compressed flag set, the data is automatically
        /// decompressed using BZ2 before being buffered.
        ///
        /// # Arguments
        /// * `message` - The StreamDataMessage to process
        ///
        /// # Returns
        /// - `Ok(true)` if the message was accepted (stream_id matched)
        /// - `Ok(false)` if the message was for a different stream
        /// - `Err(...)` if decompression failed
        pub(crate) fn receive_decompress(
            &mut self,
            message: &StreamDataMessage,
        ) -> Result<bool, CompressionError> {
            if message.stream_id != self.stream_id {
                return Ok(false);
            }

            if message.compressed {
                let decompressed = decompress_auto(&message.data, MAX_DECOMPRESS_SIZE)?;
                self.buffer.extend(&decompressed);
            } else {
                self.buffer.extend(&message.data);
            }

            if message.eof {
                self.eof = true;
            }

            Ok(true)
        }
    }

    /// Writer that applies BZ2 compression to data before sending
    ///
    /// This wrapper around `RawChannelWriter` tries to compress data and uses
    /// compression if it results in smaller output that fits within the channel MDU.
    #[derive(Debug, Clone)]
    pub struct CompressingWriter {
        inner: RawChannelWriter,
    }

    impl CompressingWriter {
        /// Create a new compressing writer
        ///
        /// # Arguments
        /// * `stream_id` - The stream identifier (0-16383)
        /// * `channel_mdu` - The channel's maximum data unit
        pub fn new(stream_id: u16, channel_mdu: usize) -> Self {
            Self {
                inner: RawChannelWriter::new(stream_id, channel_mdu),
            }
        }

        /// Get the stream ID
        pub fn stream_id(&self) -> u16 {
            self.inner.stream_id()
        }

        /// Get the maximum data length per chunk
        pub fn max_data_len(&self) -> usize {
            self.inner.max_data_len()
        }

        /// Get the inner RawChannelWriter
        #[allow(dead_code)] // Buffer API not yet integrated — see Codeberg issue #21
        pub(crate) fn inner(&self) -> &RawChannelWriter {
            &self.inner
        }

        /// Prepare a chunk without compression
        ///
        /// Use this when you don't want compression for this chunk.
        pub fn prepare_chunk(&self, data: &[u8], eof: bool) -> (StreamDataMessage, usize) {
            self.inner.prepare_chunk(data, eof)
        }

        /// Prepare a chunk with optional compression
        ///
        /// Tries to compress the data using BZ2. If compression results in smaller
        /// output that fits within max_data_len, the compressed data is used.
        /// Otherwise, uncompressed data is sent.
        ///
        /// # Arguments
        /// * `data` - Data to send
        /// * `eof` - Whether this is the last chunk
        ///
        /// # Returns
        /// A tuple of (StreamDataMessage, bytes_consumed)
        pub fn prepare_chunk_compressed(
            &self,
            data: &[u8],
            eof: bool,
        ) -> Result<(StreamDataMessage, usize), CompressionError> {
            let chunk_len = data.len().min(MAX_CHUNK_LEN);
            let chunk_data = &data[..chunk_len];

            // Try compression if data is large enough
            if chunk_len > COMPRESSION_MIN_SIZE {
                for try_num in 1..=COMPRESSION_TRIES {
                    let segment_len = chunk_len / try_num;
                    if segment_len <= COMPRESSION_MIN_SIZE {
                        break;
                    }

                    if let Ok(compressed) = compress(&data[..segment_len]) {
                        // Use compression if it's smaller and fits in max_data_len
                        if compressed.len() < segment_len && compressed.len() <= self.max_data_len()
                        {
                            let msg = self.inner.prepare_compressed_chunk(compressed, eof);
                            return Ok((msg, segment_len));
                        }
                    }
                }
            }

            // No compression or compression didn't help
            Ok(self.inner.prepare_chunk(chunk_data, eof))
        }

        /// Prepare an EOF marker message
        pub fn prepare_eof(&self) -> StreamDataMessage {
            self.inner.prepare_eof()
        }
    }
}

#[cfg(feature = "compression")]
pub use compression_support::{
    CompressingWriter, COMPRESSION_MIN_SIZE, COMPRESSION_TRIES, MAX_DECOMPRESS_SIZE,
};

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn test_raw_channel_reader_new() {
        let reader = RawChannelReader::new(42);
        assert_eq!(reader.stream_id(), 42);
        assert!(!reader.is_eof());
        assert_eq!(reader.available(), 0);
        assert!(!reader.has_data());
    }

    #[test]
    fn test_raw_channel_reader_receive() {
        let mut reader = RawChannelReader::new(1);

        // Message for different stream should be rejected
        let msg = StreamDataMessage::new(2, vec![1, 2, 3], false, false);
        assert!(!reader.receive(&msg));
        assert_eq!(reader.available(), 0);

        // Message for our stream should be accepted
        let msg = StreamDataMessage::new(1, vec![1, 2, 3], false, false);
        assert!(reader.receive(&msg));
        assert_eq!(reader.available(), 3);
        assert!(reader.has_data());
    }

    #[test]
    fn test_raw_channel_reader_receive_data() {
        let mut reader = RawChannelReader::new(0);

        reader.receive_data(&[1, 2, 3], false);
        assert_eq!(reader.available(), 3);
        assert!(!reader.is_eof());

        reader.receive_data(&[4, 5], true);
        assert_eq!(reader.available(), 5);
        assert!(reader.is_eof());
    }

    #[test]
    fn test_raw_channel_reader_eof() {
        let mut reader = RawChannelReader::new(0);

        let msg = StreamDataMessage::new(0, vec![1, 2], true, false);
        reader.receive(&msg);

        assert!(reader.is_eof());
        assert_eq!(reader.available(), 2);
    }

    #[test]
    fn test_raw_channel_reader_read() {
        let mut reader = RawChannelReader::new(0);

        let msg = StreamDataMessage::new(0, vec![1, 2, 3, 4, 5], false, false);
        reader.receive(&msg);

        let mut buf = [0u8; 3];
        assert_eq!(reader.read(&mut buf), ReadResult::Read(3));
        assert_eq!(buf, [1, 2, 3]);
        assert_eq!(reader.available(), 2);

        assert_eq!(reader.read(&mut buf), ReadResult::Read(2));
        assert_eq!(&buf[..2], [4, 5]);
    }

    #[test]
    fn test_raw_channel_reader_read_eof() {
        let mut reader = RawChannelReader::new(0);

        let msg = StreamDataMessage::new(0, vec![1], true, false);
        reader.receive(&msg);

        let mut buf = [0u8; 10];
        assert_eq!(reader.read(&mut buf), ReadResult::Read(1));

        // After consuming all data with EOF, should return Eof
        assert_eq!(reader.read(&mut buf), ReadResult::Eof);
    }

    #[test]
    fn test_raw_channel_reader_would_block() {
        let mut reader = RawChannelReader::new(0);

        let mut buf = [0u8; 10];
        assert_eq!(reader.read(&mut buf), ReadResult::WouldBlock);
    }

    #[test]
    fn test_raw_channel_reader_read_all() {
        let mut reader = RawChannelReader::new(0);

        let msg = StreamDataMessage::new(0, vec![1, 2, 3, 4, 5], false, false);
        reader.receive(&msg);

        let data = reader.read_all();
        assert_eq!(data, vec![1, 2, 3, 4, 5]);
        assert_eq!(reader.available(), 0);
    }

    #[test]
    fn test_raw_channel_reader_peek() {
        let mut reader = RawChannelReader::new(0);

        let msg = StreamDataMessage::new(0, vec![1, 2, 3], false, false);
        reader.receive(&msg);

        let mut buf = [0u8; 2];
        assert_eq!(reader.peek(&mut buf), 2);
        assert_eq!(buf, [1, 2]);

        // Data should still be there
        assert_eq!(reader.available(), 3);
    }

    #[test]
    fn test_raw_channel_reader_reset() {
        let mut reader = RawChannelReader::new(0);

        let msg = StreamDataMessage::new(0, vec![1, 2, 3], true, false);
        reader.receive(&msg);

        assert!(reader.is_eof());
        assert_eq!(reader.available(), 3);

        reader.reset();
        assert!(!reader.is_eof());
        assert_eq!(reader.available(), 0);
    }

    #[test]
    fn test_raw_channel_reader_clear_buffer() {
        let mut reader = RawChannelReader::new(0);

        let msg = StreamDataMessage::new(0, vec![1, 2, 3], true, false);
        reader.receive(&msg);

        assert!(reader.is_eof());
        assert_eq!(reader.available(), 3);

        // clear_buffer should clear data but preserve EOF
        reader.clear_buffer();
        assert!(reader.is_eof()); // EOF preserved
        assert_eq!(reader.available(), 0); // Data cleared
    }

    #[test]
    fn test_raw_channel_writer_new() {
        let writer = RawChannelWriter::new(5, 458);
        assert_eq!(writer.stream_id(), 5);
        // max_data_len = 458 - 2 (stream header) = 456
        assert_eq!(writer.max_data_len(), 456);
    }

    #[test]
    fn test_raw_channel_writer_prepare_chunk() {
        let writer = RawChannelWriter::new(1, 100);

        let data = vec![0xAA; 50];
        let (msg, consumed) = writer.prepare_chunk(&data, false);

        assert_eq!(msg.stream_id, 1);
        assert!(!msg.eof);
        assert!(!msg.compressed);
        assert_eq!(consumed, 50);
        assert_eq!(msg.data.len(), 50);
    }

    #[test]
    fn test_raw_channel_writer_prepare_chunk_truncates() {
        let writer = RawChannelWriter::new(1, 20); // max_data_len = 18

        let data = vec![0xBB; 100];
        let (msg, consumed) = writer.prepare_chunk(&data, false);

        // Should be limited to max_data_len
        assert_eq!(consumed, 18);
        assert_eq!(msg.data.len(), 18);
    }

    #[test]
    #[cfg(feature = "compression")]
    fn test_raw_channel_writer_prepare_compressed_chunk() {
        let writer = RawChannelWriter::new(1, 100);

        let compressed_data = vec![0xCC; 30];
        let msg = writer.prepare_compressed_chunk(compressed_data.clone(), true);

        assert_eq!(msg.stream_id, 1);
        assert!(msg.eof);
        assert!(msg.compressed);
        assert_eq!(msg.data, compressed_data);
    }

    #[test]
    fn test_raw_channel_writer_prepare_eof() {
        let writer = RawChannelWriter::new(99, 100);
        let msg = writer.prepare_eof();

        assert_eq!(msg.stream_id, 99);
        assert!(msg.eof);
        assert!(msg.data.is_empty());
    }

    #[test]
    fn test_buffered_channel_writer_new() {
        let writer = BufferedChannelWriter::new(0, 50);
        assert_eq!(writer.stream_id(), 0);
        assert_eq!(writer.buffered(), 0);
        assert!(!writer.has_pending());
    }

    #[test]
    fn test_buffered_channel_writer_write() {
        let mut writer = BufferedChannelWriter::new(0, 50);

        let n = writer.write(b"hello");
        assert_eq!(n, 5);
        assert_eq!(writer.buffered(), 5);

        let n = writer.write(b" world");
        assert_eq!(n, 6);
        assert_eq!(writer.buffered(), 11);
    }

    #[test]
    fn test_buffered_channel_writer_take_pending() {
        let mut writer = BufferedChannelWriter::new(0, 50);

        writer.write(b"hello world");
        writer.finish();

        let messages = writer.take_pending();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].stream_id, 0);
        assert!(messages[0].eof);
        assert!(!messages[0].compressed);
        assert_eq!(messages[0].data, b"hello world");
    }

    #[test]
    fn test_buffered_channel_writer_chunking() {
        let mut writer = BufferedChannelWriter::new(0, 12); // max_data_len = 10

        writer.write(b"12345678901234567890"); // 20 bytes
        writer.finish();

        let messages = writer.take_pending();

        // Should be chunked into multiple messages
        assert!(messages.len() >= 2);

        // Last message should have EOF
        assert!(messages.last().unwrap().eof);

        // Total data should match
        let total: Vec<u8> = messages
            .iter()
            .flat_map(|m| m.data.iter().copied())
            .collect();
        assert_eq!(total, b"12345678901234567890");
    }

    #[test]
    fn test_buffered_channel_writer_clear() {
        let mut writer = BufferedChannelWriter::new(0, 50);

        writer.write(b"test data");
        writer.finish();

        assert!(writer.has_pending());

        writer.clear();

        assert!(!writer.has_pending());
        assert_eq!(writer.buffered(), 0);
        assert!(!writer.is_finished());
    }

    #[test]
    fn test_max_data_len() {
        // channel_mdu = 458 (link_mdu - envelope_header)
        // max_data_len = 458 - 2 (stream_header) = 456
        assert_eq!(max_data_len(458), 456);
    }

    #[test]
    fn test_stream_overhead() {
        // 2 bytes stream header + 6 bytes envelope header = 8
        assert_eq!(stream_overhead(), 8);
    }

    #[test]
    fn test_buffered_channel_writer_take_pending_twice() {
        let mut writer = BufferedChannelWriter::new(0, 50);

        writer.write(b"hello");
        writer.finish();

        // First take_pending should return the message with EOF
        let messages = writer.take_pending();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].eof);
        assert_eq!(messages[0].data, b"hello");

        // Second take_pending should return empty (EOF already sent)
        let messages = writer.take_pending();
        assert!(messages.is_empty());
    }

    #[test]
    fn test_buffered_channel_writer_write_clear_take_pending() {
        let mut writer = BufferedChannelWriter::new(0, 50);

        writer.write(b"hello world");
        assert_eq!(writer.buffered(), 11);

        // Clear without finish
        writer.clear();
        assert_eq!(writer.buffered(), 0);
        assert!(!writer.is_finished());

        // take_pending should return empty since buffer is clear and not finished
        let messages = writer.take_pending();
        assert!(messages.is_empty());
    }

    #[test]
    fn test_buffered_channel_writer_finish_without_data() {
        let mut writer = BufferedChannelWriter::new(0, 50);

        // Finish without writing any data
        writer.finish();

        // Should get an EOF marker message
        let messages = writer.take_pending();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].eof);
        assert!(messages[0].data.is_empty());
    }
}

#[cfg(all(test, feature = "compression"))]
mod compression_tests {
    use super::*;
    use crate::compression::compress;
    use alloc::vec;
    use alloc::vec::Vec;

    #[test]
    fn test_receive_decompress_uncompressed() {
        let mut reader = RawChannelReader::new(0);

        // Uncompressed message
        let msg = StreamDataMessage::new(0, vec![1, 2, 3], false, false);
        let accepted = reader.receive_decompress(&msg).unwrap();

        assert!(accepted);
        assert_eq!(reader.available(), 3);
    }

    #[test]
    fn test_receive_decompress_compressed() {
        let mut reader = RawChannelReader::new(0);

        // Compress some data
        let original = b"Test data for compression";
        let compressed = compress(original).expect("compress failed");

        let msg = StreamDataMessage::new(0, compressed, false, true);
        let accepted = reader.receive_decompress(&msg).unwrap();

        assert!(accepted);
        assert_eq!(reader.available(), original.len());

        let data = reader.read_all();
        assert_eq!(data, original);
    }

    #[test]
    fn test_receive_decompress_wrong_stream() {
        let mut reader = RawChannelReader::new(0);

        let msg = StreamDataMessage::new(1, vec![1, 2, 3], false, false);
        let accepted = reader.receive_decompress(&msg).unwrap();

        assert!(!accepted);
        assert_eq!(reader.available(), 0);
    }

    #[test]
    fn test_receive_decompress_eof() {
        let mut reader = RawChannelReader::new(0);

        let original = b"Last chunk";
        let compressed = compress(original).expect("compress failed");

        let msg = StreamDataMessage::new(0, compressed, true, true);
        reader.receive_decompress(&msg).unwrap();

        assert!(reader.is_eof());
    }

    #[test]
    fn test_compressing_writer_new() {
        let writer = CompressingWriter::new(5, 458);
        assert_eq!(writer.stream_id(), 5);
        assert_eq!(writer.max_data_len(), 456);
    }

    #[test]
    fn test_compressing_writer_small_data() {
        let writer = CompressingWriter::new(0, 500);

        // Small data shouldn't be compressed
        let data = vec![b'X'; 10];
        let (msg, consumed) = writer.prepare_chunk_compressed(&data, false).unwrap();

        assert!(!msg.compressed);
        assert_eq!(consumed, 10);
    }

    #[test]
    fn test_compressing_writer_compressible_data() {
        let writer = CompressingWriter::new(0, 500);

        // Highly compressible data
        let data: Vec<u8> = (0..200).map(|_| b'A').collect();
        let (msg, consumed) = writer.prepare_chunk_compressed(&data, false).unwrap();

        // Should have compressed the data
        if msg.compressed {
            assert!(msg.data.len() < consumed);
        }
        assert!(consumed > 0);
    }

    #[test]
    fn test_compressing_writer_roundtrip() {
        let writer = CompressingWriter::new(0, 500);
        let mut reader = RawChannelReader::new(0);

        // Create data that compresses well
        let original: Vec<u8> = (0..500).map(|_| b'A').collect();
        let (msg, consumed) = writer.prepare_chunk_compressed(&original, true).unwrap();

        // Receive and decompress
        reader.receive_decompress(&msg).unwrap();

        let decompressed = reader.read_all();
        assert_eq!(decompressed, &original[..consumed]);
    }

    #[test]
    fn test_compressing_writer_eof() {
        let writer = CompressingWriter::new(0, 500);
        let msg = writer.prepare_eof();

        assert_eq!(msg.stream_id, 0);
        assert!(msg.eof);
        assert!(msg.data.is_empty());
    }
}
