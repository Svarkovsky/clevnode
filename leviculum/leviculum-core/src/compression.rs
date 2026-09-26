//! BZ2 compression for no_std environments
//!
//! This module provides safe wrappers around libbz2-rs-sys (a pure Rust bzip2
//! implementation) for BZ2 compression and decompression. The underlying
//! library is 100% Rust with no C dependencies, the `unsafe` blocks bridge
//! between Rust slices and the library's C-compatible pointer API.
//!
//! # Example
//!
//! ```
//! use leviculum_core::compression::{compress, decompress};
//!
//! let data = b"Hello, world! This is some data to compress.";
//! let compressed = compress(data).unwrap();
//! let decompressed = decompress(&compressed, 1024).unwrap();
//! assert_eq!(&decompressed, data);
//! ```
//!
//! # Wire Compatibility
//!
//! The compression format is standard BZ2, compatible with Python's bz2 module
//! and the Python Reticulum implementation.

use alloc::vec;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int, c_uint};

use libbz2_rs_sys::{
    BZ2_bzBuffToBuffCompress, BZ2_bzBuffToBuffDecompress, BZ_DATA_ERROR, BZ_DATA_ERROR_MAGIC,
    BZ_MEM_ERROR, BZ_OK, BZ_OUTBUFF_FULL, BZ_PARAM_ERROR,
};

/// Compression error types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionError {
    /// Output buffer too small for compressed/decompressed data
    OutputTooSmall,
    /// Compressed data is corrupt or invalid
    DataCorrupt,
    /// Invalid BZ2 magic number in compressed data
    InvalidMagic,
    /// Memory allocation failed
    OutOfMemory,
    /// Invalid parameters
    InvalidParam,
    /// Unexpected error from compression library
    Unexpected(c_int),
}

impl core::fmt::Display for CompressionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OutputTooSmall => write!(f, "output buffer too small"),
            Self::DataCorrupt => write!(f, "compressed data is corrupt"),
            Self::InvalidMagic => write!(f, "invalid BZ2 magic number"),
            Self::OutOfMemory => write!(f, "memory allocation failed"),
            Self::InvalidParam => write!(f, "invalid parameters"),
            Self::Unexpected(code) => write!(f, "unexpected error code: {}", code),
        }
    }
}

/// BZ2 block size (1-9, where 9 = best compression but more memory)
/// Using 9 (900k block size) like Python Reticulum
const BLOCK_SIZE_100K: c_int = 9;

/// Work factor (0 = default of 30)
const WORK_FACTOR: c_int = 0;

/// Verbosity level (0 = silent)
const VERBOSITY: c_int = 0;

/// Small decompression flag (0 = normal, 1 = low memory)
const SMALL_DECOMPRESS: c_int = 0;

/// Estimate worst-case compressed size for a given input length.
///
/// BZ2 can expand data slightly for incompressible input.
/// Formula: source_len + ceil(source_len/100) + 600 (per bzip2 documentation)
fn max_compressed_size(source_len: usize) -> usize {
    source_len + (source_len / 100) + 600 + 1
}

/// Compress data using BZ2
///
/// # Arguments
/// * `data` - The data to compress
///
/// # Returns
/// Compressed data or a compression error
///
/// # Example
///
/// ```
/// use leviculum_core::compression::compress;
///
/// let compressed = compress(b"Hello, world!").unwrap();
/// assert!(!compressed.is_empty());
/// ```
pub fn compress(data: &[u8]) -> Result<Vec<u8>, CompressionError> {
    if data.is_empty() {
        // Return empty BZ2 stream for empty input
        return compress_with_size(data, max_compressed_size(0));
    }

    compress_with_size(data, max_compressed_size(data.len()))
}

/// Compress data with a specific output buffer size
fn compress_with_size(data: &[u8], dest_size: usize) -> Result<Vec<u8>, CompressionError> {
    let mut dest = vec![0u8; dest_size];

    let mut dest_len = dest_size as c_uint;

    // SAFETY:
    // - dest and source are valid pointers to allocated memory
    // - dest_len is valid and points to a valid c_uint
    // - source_len is within bounds of source
    // - All parameters are valid values
    let ret = unsafe {
        BZ2_bzBuffToBuffCompress(
            dest.as_mut_ptr() as *mut c_char,
            &mut dest_len,
            data.as_ptr() as *mut c_char,
            data.len() as c_uint,
            BLOCK_SIZE_100K,
            VERBOSITY,
            WORK_FACTOR,
        )
    };

    match ret {
        BZ_OK => {
            // dest_len is updated to the actual compressed size
            dest.truncate(dest_len as usize);
            Ok(dest)
        }
        BZ_OUTBUFF_FULL => Err(CompressionError::OutputTooSmall),
        BZ_MEM_ERROR => Err(CompressionError::OutOfMemory),
        BZ_PARAM_ERROR => Err(CompressionError::InvalidParam),
        code => Err(CompressionError::Unexpected(code)),
    }
}

/// Decompress BZ2 data
///
/// # Arguments
/// * `data` - BZ2 compressed data
/// * `max_size` - Maximum expected decompressed size (for buffer allocation)
///
/// # Returns
/// Decompressed data or a decompression error
///
/// # Example
///
/// ```
/// use leviculum_core::compression::{compress, decompress};
///
/// let compressed = compress(b"Hello, world!").unwrap();
/// let decompressed = decompress(&compressed, 1024).unwrap();
/// assert_eq!(&decompressed, b"Hello, world!");
/// ```
pub fn decompress(data: &[u8], max_size: usize) -> Result<Vec<u8>, CompressionError> {
    if data.is_empty() {
        return Ok(Vec::new());
    }

    let mut dest = vec![0u8; max_size];

    let mut dest_len = max_size as c_uint;

    // SAFETY:
    // - dest and source are valid pointers to allocated memory
    // - dest_len is valid and points to a valid c_uint
    // - source_len is within bounds of source
    // - All parameters are valid values
    let ret = unsafe {
        BZ2_bzBuffToBuffDecompress(
            dest.as_mut_ptr() as *mut c_char,
            &mut dest_len,
            data.as_ptr() as *mut c_char,
            data.len() as c_uint,
            SMALL_DECOMPRESS,
            VERBOSITY,
        )
    };

    match ret {
        BZ_OK => {
            // dest_len is updated to the actual decompressed size
            dest.truncate(dest_len as usize);
            Ok(dest)
        }
        BZ_OUTBUFF_FULL => Err(CompressionError::OutputTooSmall),
        BZ_DATA_ERROR => Err(CompressionError::DataCorrupt),
        BZ_DATA_ERROR_MAGIC => Err(CompressionError::InvalidMagic),
        BZ_MEM_ERROR => Err(CompressionError::OutOfMemory),
        BZ_PARAM_ERROR => Err(CompressionError::InvalidParam),
        code => Err(CompressionError::Unexpected(code)),
    }
}

/// Decompress BZ2 data with automatic buffer sizing
///
/// Starts with an initial estimate and grows the buffer if needed.
/// Use this when you don't know the decompressed size.
///
/// # Arguments
/// * `data` - BZ2 compressed data
/// * `max_size` - Maximum allowed decompressed size (safety limit)
///
/// # Returns
/// Decompressed data or a decompression error
pub fn decompress_auto(data: &[u8], max_size: usize) -> Result<Vec<u8>, CompressionError> {
    if data.is_empty() {
        return Ok(Vec::new());
    }

    // Start with 2x compressed size as initial estimate, minimum 1KB
    let mut estimate = (data.len() * 2).max(1024);

    loop {
        if estimate > max_size {
            estimate = max_size;
        }

        match decompress(data, estimate) {
            Ok(result) => return Ok(result),
            Err(CompressionError::OutputTooSmall) => {
                if estimate >= max_size {
                    return Err(CompressionError::OutputTooSmall);
                }
                // Double the estimate, capped at max_size
                estimate = (estimate * 2).min(max_size);
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::vec::Vec;

    #[test]
    fn test_compress_decompress_roundtrip() {
        let original = b"Hello, world! This is a test of BZ2 compression.";

        let compressed = compress(original).expect("compress failed");
        assert!(!compressed.is_empty());

        // BZ2 data starts with "BZ" magic
        assert_eq!(&compressed[0..2], b"BZ");

        let decompressed = decompress(&compressed, original.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_compress_empty() {
        let compressed = compress(b"").expect("compress empty failed");
        // Empty BZ2 stream is valid
        assert!(!compressed.is_empty());
    }

    #[test]
    fn test_decompress_empty() {
        let result = decompress(b"", 1024).expect("decompress empty failed");
        assert!(result.is_empty());
    }

    #[test]
    fn test_decompress_invalid_magic() {
        let result = decompress(b"not bz2 data", 1024);
        assert_eq!(result, Err(CompressionError::InvalidMagic));
    }

    #[test]
    fn test_decompress_corrupt() {
        // Valid BZ2 header but corrupt data
        let corrupt = b"BZh91AY&SY\x00\x00\x00\x00";
        let result = decompress(corrupt, 1024);
        assert!(matches!(
            result,
            Err(CompressionError::DataCorrupt | CompressionError::Unexpected(_))
        ));
    }

    #[test]
    fn test_highly_compressible() {
        // Highly compressible data (repeated pattern)
        let original: Vec<u8> = (0..1000).map(|_| b'A').collect();

        let compressed = compress(&original).expect("compress failed");
        // Should compress significantly
        assert!(compressed.len() < original.len());

        let decompressed = decompress(&compressed, original.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_decompress_auto() {
        let original = b"Test data for auto decompression sizing";

        let compressed = compress(original).expect("compress failed");

        // Auto-size should work without knowing exact size
        let decompressed =
            decompress_auto(&compressed, 1024 * 1024).expect("decompress_auto failed");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_decompress_auto_max_limit() {
        let original: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();
        let compressed = compress(&original).expect("compress failed");

        // Max size too small should fail
        let result = decompress_auto(&compressed, 100);
        assert_eq!(result, Err(CompressionError::OutputTooSmall));
    }

    #[test]
    fn test_various_sizes() {
        // Test various data sizes
        for size in [1, 10, 100, 500, 1000, 5000] {
            let original: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();

            let compressed = compress(&original).expect("compress failed");
            let decompressed =
                decompress(&compressed, original.len() * 2).expect("decompress failed");

            assert_eq!(decompressed, original, "Failed for size {}", size);
        }
    }

    #[test]
    fn test_compression_error_display() {
        assert_eq!(
            format!("{}", CompressionError::OutputTooSmall),
            "output buffer too small"
        );
        assert_eq!(
            format!("{}", CompressionError::DataCorrupt),
            "compressed data is corrupt"
        );
        assert_eq!(
            format!("{}", CompressionError::InvalidMagic),
            "invalid BZ2 magic number"
        );
        assert_eq!(
            format!("{}", CompressionError::OutOfMemory),
            "memory allocation failed"
        );
        assert_eq!(
            format!("{}", CompressionError::InvalidParam),
            "invalid parameters"
        );
        assert_eq!(
            format!("{}", CompressionError::Unexpected(-99)),
            "unexpected error code: -99"
        );
    }
}
