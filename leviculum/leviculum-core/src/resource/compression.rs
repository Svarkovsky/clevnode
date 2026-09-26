//! Bzip2 compression/decompression wrappers for Resource transfers.
//!
//! Gated behind the `compression` cargo feature (uses `libbz2-rs-sys`).
//! Python Reticulum uses `bz2.compress()` / `bz2.decompress()`.

#[cfg(feature = "compression")]
use super::ResourceError;
#[cfg(feature = "compression")]
use alloc::vec;
#[cfg(feature = "compression")]
use alloc::vec::Vec;

/// Compress data using bzip2 (matching Python's `bz2.compress(data)`).
///
/// Uses block size 9 (900k) and work factor 0 (default = 30), matching
/// Python's default bz2 compression settings.
#[cfg(feature = "compression")]
pub(crate) fn bz2_compress(input: &[u8]) -> Result<Vec<u8>, ResourceError> {
    use core::ffi::{c_char, c_int, c_uint};

    // bzip2 worst-case expansion is ~1.01x + 600 bytes
    let max_output = input.len() + input.len() / 100 + 600 + 1;
    let mut output = vec![0u8; max_output];
    let mut dest_len: c_uint = max_output as c_uint;

    let ret = unsafe {
        libbz2_rs_sys::BZ2_bzBuffToBuffCompress(
            output.as_mut_ptr() as *mut c_char,
            &mut dest_len,
            input.as_ptr() as *mut c_char,
            input.len() as c_uint,
            9 as c_int, // blockSize100k = 9 (Python default)
            0 as c_int, // verbosity = 0
            0 as c_int, // workFactor = 0 (default = 30)
        )
    };

    if ret != libbz2_rs_sys::BZ_OK {
        return Err(ResourceError::CompressionFailed);
    }

    output.truncate(dest_len as usize);
    Ok(output)
}

/// Largest first-attempt output buffer `bz2_decompress` will size from its
/// caller-supplied hint.
///
/// The hint comes from the advertisement's `data_size` field, which a peer
/// chooses freely and which is the total across *all* segments of a split
/// transfer, so it is neither trustworthy nor per-segment accurate. One
/// segment's plaintext never exceeds `MAX_EFFICIENT_SIZE` (Resource.py:116,
/// :296-313), so clamping here only ever shrinks an over-estimate; an
/// under-estimate is absorbed by the retry-doubling loop below (Codeberg
/// #263, item 3).
#[cfg(feature = "compression")]
const MAX_DECOMPRESS_HINT: usize = super::RESOURCE_MAX_EFFICIENT_SIZE;

/// Decompress bzip2 data (matching Python's `bz2.decompress(data)`).
///
/// `expected_size` is only a hint for the output buffer size (from the
/// advertisement's `data_size` field). It is clamped to
/// [`MAX_DECOMPRESS_HINT`] so an untrusted field can never size the
/// allocation directly; the retry loop grows the buffer when the real output
/// is larger.
#[cfg(feature = "compression")]
pub(crate) fn bz2_decompress(input: &[u8], expected_size: usize) -> Result<Vec<u8>, ResourceError> {
    use core::ffi::{c_char, c_int, c_uint};

    // Start with the (clamped) expected size + margin, retry with a larger
    // buffer if needed.
    let expected_size = expected_size.min(MAX_DECOMPRESS_HINT);
    let mut buf_size = expected_size.saturating_add(expected_size / 10).max(1024);

    for _ in 0..4 {
        let mut output = vec![0u8; buf_size];
        let mut dest_len: c_uint = buf_size as c_uint;

        let ret = unsafe {
            libbz2_rs_sys::BZ2_bzBuffToBuffDecompress(
                output.as_mut_ptr() as *mut c_char,
                &mut dest_len,
                input.as_ptr() as *mut c_char,
                input.len() as c_uint,
                0 as c_int, // small = 0 (use normal algorithm)
                0 as c_int, // verbosity = 0
            )
        };

        if ret == libbz2_rs_sys::BZ_OK {
            output.truncate(dest_len as usize);
            return Ok(output);
        }

        if ret == libbz2_rs_sys::BZ_OUTBUFF_FULL {
            buf_size = buf_size.saturating_mul(2);
            continue;
        }

        return Err(ResourceError::DecompressionFailed);
    }

    Err(ResourceError::DecompressionFailed)
}

#[cfg(test)]
#[cfg(feature = "compression")]
mod tests {
    use super::*;

    #[test]
    fn test_compress_decompress_roundtrip() {
        let data = b"Hello, this is test data for bzip2 compression!";
        let compressed = bz2_compress(data).unwrap();
        let decompressed = bz2_decompress(&compressed, data.len()).unwrap();
        assert_eq!(&decompressed, data);
    }

    #[test]
    fn test_compress_actually_compresses_repetitive_data() {
        let data = vec![0x42u8; 10000];
        let compressed = bz2_compress(&data).unwrap();
        assert!(
            compressed.len() < data.len(),
            "compressed {} vs original {}",
            compressed.len(),
            data.len()
        );
    }

    /// Codeberg #263 item 3: a hostile `data_size` must not size the output
    /// buffer. The hint is clamped, so a peer claiming a 4 GiB decompressed
    /// size gets a bounded first allocation (and, since the payload is not
    /// really that large, a plain error rather than an out-of-memory abort).
    #[test]
    fn decompress_hint_is_clamped() {
        let data = vec![0x42u8; 4096];
        let compressed = bz2_compress(&data).unwrap();

        // The hint the peer would have chosen for us, pre-fix, was used raw.
        let decompressed = bz2_decompress(&compressed, usize::MAX).unwrap();
        assert_eq!(decompressed, data);
    }

    /// Verifies the claim the clamp rests on: the retry-doubling loop absorbs
    /// an under-estimated hint, so a resource whose real decompressed size
    /// exceeds the clamp still transfers correctly.
    #[test]
    fn decompress_output_larger_than_the_clamp_still_succeeds() {
        // Compressible payload well past MAX_DECOMPRESS_HINT.
        let mut data = vec![0u8; 3 * MAX_DECOMPRESS_HINT];
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }
        let compressed = bz2_compress(&data).unwrap();

        // Both the hostile over-estimate and a truthful hint end up clamped to
        // the same starting buffer, which is smaller than the real output.
        let decompressed = bz2_decompress(&compressed, usize::MAX).unwrap();
        assert_eq!(decompressed.len(), data.len());
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_decompress_bad_data() {
        let result = bz2_decompress(b"not valid bz2", 100);
        assert!(result.is_err());
    }

    #[test]
    fn test_compress_empty() {
        let compressed = bz2_compress(b"").unwrap();
        let decompressed = bz2_decompress(&compressed, 0).unwrap();
        assert!(decompressed.is_empty());
    }
}
