//! Identity handle and its operations.
//!
//! `lev_identity_t` is an opaque handle around a `leviculum_std::api::Identity`.
//! Keys cross the boundary read(2) style. See `docs/leviculum-api-design.md`.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int};

use leviculum_std::api::Identity;
use zeroize::Zeroizing;

use crate::error::*;
use crate::{guard, write_out};

/// Length of a destination or identity hash, in bytes.
pub const LEV_ADDR_LEN: usize = 16;
/// Length of a combined key, public or private, in bytes. The layout is the
/// X25519 key in bytes 0..32 then the Ed25519 key in bytes 32..64.
pub const LEV_IDENTITY_KEY_LEN: usize = 64;
/// Length of the X25519 (encryption) half, bytes 0..32 of a combined key.
pub const LEV_X25519_KEY_LEN: usize = 32;
/// Length of the Ed25519 (signing) half, bytes 32..64 of a combined key. This
/// is the key a link needs; see `lev_connect_with_key`.
pub const LEV_SIGNING_KEY_LEN: usize = 32;

/// Opaque identity handle.
pub struct lev_identity_t {
    pub(crate) inner: Identity,
}

/// Generate a new random identity. Returns NULL on failure.
#[no_mangle]
pub extern "C" fn lev_identity_generate() -> *mut lev_identity_t {
    guard(std::ptr::null_mut(), || {
        let inner = leviculum_std::api::generate_identity();
        Box::into_raw(Box::new(lev_identity_t { inner }))
    })
}

/// Load a full identity from its 64-byte combined private key.
///
/// `len` must equal `LEV_IDENTITY_KEY_LEN`. Returns NULL on failure.
#[no_mangle]
pub unsafe extern "C" fn lev_identity_from_private_key(
    key: *const u8,
    len: usize,
) -> *mut lev_identity_t {
    guard(std::ptr::null_mut(), || {
        if key.is_null() || len != LEV_IDENTITY_KEY_LEN {
            set_last_error("private key must be 64 bytes");
            return std::ptr::null_mut();
        }
        let bytes = std::slice::from_raw_parts(key, len);
        match Identity::from_private_key_bytes(bytes) {
            Ok(inner) => Box::into_raw(Box::new(lev_identity_t { inner })),
            Err(e) => {
                set_last_error(format!("{e:?}"));
                std::ptr::null_mut()
            }
        }
    })
}

/// Load a public-only identity from its 64-byte combined public key.
///
/// `len` must equal `LEV_IDENTITY_KEY_LEN`. Returns NULL on failure.
#[no_mangle]
pub unsafe extern "C" fn lev_identity_from_public_key(
    key: *const u8,
    len: usize,
) -> *mut lev_identity_t {
    guard(std::ptr::null_mut(), || {
        if key.is_null() || len != LEV_IDENTITY_KEY_LEN {
            set_last_error("public key must be 64 bytes");
            return std::ptr::null_mut();
        }
        let bytes = std::slice::from_raw_parts(key, len);
        match Identity::from_public_key_bytes(bytes) {
            Ok(inner) => Box::into_raw(Box::new(lev_identity_t { inner })),
            Err(e) => {
                set_last_error(format!("{e:?}"));
                std::ptr::null_mut()
            }
        }
    })
}

/// Free an identity handle. `lev_identity_free(NULL)` is a no-op.
#[no_mangle]
pub unsafe extern "C" fn lev_identity_free(id: *mut lev_identity_t) {
    guard((), || {
        if !id.is_null() {
            drop(Box::from_raw(id));
        }
    })
}

/// Write the identity hash (16 bytes) into `buf`, read(2) style.
#[no_mangle]
pub unsafe extern "C" fn lev_identity_hash(
    id: *const lev_identity_t,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let id = match id.as_ref() {
            Some(id) => id,
            None => return LEV_ERR_NULL_PTR,
        };
        write_out(id.inner.hash(), buf, cap, out_len)
    })
}

/// Write the combined public key (64 bytes) into `buf`, read(2) style.
#[no_mangle]
pub unsafe extern "C" fn lev_identity_public_key(
    id: *const lev_identity_t,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let id = match id.as_ref() {
            Some(id) => id,
            None => return LEV_ERR_NULL_PTR,
        };
        write_out(&id.inner.public_key_bytes(), buf, cap, out_len)
    })
}

/// Write the combined private key (64 bytes) into `buf`, read(2) style.
///
/// Returns `LEV_ERR_CRYPTO` if the identity is public-only.
#[no_mangle]
pub unsafe extern "C" fn lev_identity_private_key(
    id: *const lev_identity_t,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let id = match id.as_ref() {
            Some(id) => id,
            None => return LEV_ERR_NULL_PTR,
        };
        match id.inner.private_key_bytes() {
            // Zeroize the transient copy of the secret key once written out.
            Ok(key) => write_out(Zeroizing::new(key).as_slice(), buf, cap, out_len),
            Err(e) => {
                set_last_error(format!("{e:?}"));
                LEV_ERR_CRYPTO
            }
        }
    })
}

/// Return 1 if the identity holds private keys, 0 otherwise (also 0 on NULL).
#[no_mangle]
pub unsafe extern "C" fn lev_identity_has_private_keys(id: *const lev_identity_t) -> c_int {
    guard(0, || match id.as_ref() {
        Some(id) if id.inner.has_private_keys() => 1,
        _ => 0,
    })
}

/// Borrow a C path string as `&str`, or `None` if NULL or not valid UTF-8.
unsafe fn path_str<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok()
}

/// Save the identity's private key to `path` as raw 64 bytes, the format Python
/// Reticulum uses (`rnsd` and friends read it). Requires private keys.
/// Written atomically (temp file then rename).
#[no_mangle]
pub unsafe extern "C" fn lev_identity_save_file(
    id: *const lev_identity_t,
    path: *const c_char,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let id = match id.as_ref() {
            Some(id) => id,
            None => return LEV_ERR_NULL_PTR,
        };
        let path = match path_str(path) {
            Some(p) => p,
            None => return LEV_ERR_INVALID_ARG,
        };
        // Zeroize the transient copy of the secret key on drop.
        let bytes = match id.inner.private_key_bytes() {
            Ok(b) => Zeroizing::new(b),
            Err(e) => {
                set_last_error(format!("{e:?}"));
                return LEV_ERR_CRYPTO;
            }
        };
        let tmp = format!("{path}.tmp");
        if let Err(e) = std::fs::write(&tmp, bytes.as_slice()) {
            set_last_error(format!("write {tmp}: {e}"));
            return LEV_ERR_IO;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            set_last_error(format!("rename to {path}: {e}"));
            return LEV_ERR_IO;
        }
        LEV_OK
    })
}

/// Load a full identity from a raw 64-byte private key file (Python-compatible
/// format). Returns NULL on failure (missing file, wrong size, or bad keys).
#[no_mangle]
pub unsafe extern "C" fn lev_identity_load_file(path: *const c_char) -> *mut lev_identity_t {
    guard(std::ptr::null_mut(), || {
        let path = match path_str(path) {
            Some(p) => p,
            None => {
                set_last_error("path must be a valid UTF-8 string");
                return std::ptr::null_mut();
            }
        };
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                set_last_error(format!("read {path}: {e}"));
                return std::ptr::null_mut();
            }
        };
        match Identity::from_private_key_bytes(&bytes) {
            Ok(inner) => Box::into_raw(Box::new(lev_identity_t { inner })),
            Err(e) => {
                set_last_error(format!("{e:?}"));
                std::ptr::null_mut()
            }
        }
    })
}

/// Build a slice from a (ptr, len) pair; an empty length yields an empty slice
/// even for a NULL pointer, otherwise NULL is rejected by the caller.
unsafe fn bytes<'a>(data: *const u8, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        Some(&[])
    } else if data.is_null() {
        None
    } else {
        Some(std::slice::from_raw_parts(data, len))
    }
}

/// Sign `msg` with the identity's Ed25519 private key. Writes the 64-byte
/// signature read(2) style (NULL `buf` queries the length). `LEV_ERR_CRYPTO`
/// if the identity has no private key.
#[no_mangle]
pub unsafe extern "C" fn lev_identity_sign(
    id: *const lev_identity_t,
    msg: *const u8,
    msg_len: usize,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let id = match id.as_ref() {
            Some(id) => id,
            None => return LEV_ERR_NULL_PTR,
        };
        let msg = match bytes(msg, msg_len) {
            Some(m) => m,
            None => return LEV_ERR_NULL_PTR,
        };
        match id.inner.sign(msg) {
            Ok(sig) => write_out(&sig, buf, cap, out_len),
            Err(e) => {
                set_last_error(format!("{e:?}"));
                LEV_ERR_CRYPTO
            }
        }
    })
}

/// Verify `sig` over `msg` against the identity's Ed25519 public key. Returns 1
/// if valid, 0 if the signature does not match, and a negative `LEV_ERR_*` on a
/// NULL argument or a malformed signature.
#[no_mangle]
pub unsafe extern "C" fn lev_identity_verify(
    id: *const lev_identity_t,
    msg: *const u8,
    msg_len: usize,
    sig: *const u8,
    sig_len: usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let id = match id.as_ref() {
            Some(id) => id,
            None => return LEV_ERR_NULL_PTR,
        };
        let msg = match bytes(msg, msg_len) {
            Some(m) => m,
            None => return LEV_ERR_NULL_PTR,
        };
        let sig = match bytes(sig, sig_len) {
            Some(s) => s,
            None => return LEV_ERR_NULL_PTR,
        };
        match id.inner.verify(msg, sig) {
            Ok(true) => 1,
            Ok(false) => 0,
            Err(e) => {
                set_last_error(format!("{e:?}"));
                LEV_ERR_INVALID_ARG
            }
        }
    })
}

/// Encrypt `plaintext` to the identity's public key (X25519 + AES, the
/// Reticulum scheme). Writes the ciphertext read(2) style (NULL `buf` queries
/// the length). Encryption is randomised, so a length query and the real call
/// produce different bytes of the same length; size from the query, then call
/// again with a buffer.
#[no_mangle]
pub unsafe extern "C" fn lev_identity_encrypt(
    id: *const lev_identity_t,
    plaintext: *const u8,
    plaintext_len: usize,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let id = match id.as_ref() {
            Some(id) => id,
            None => return LEV_ERR_NULL_PTR,
        };
        let plaintext = match bytes(plaintext, plaintext_len) {
            Some(p) => p,
            None => return LEV_ERR_NULL_PTR,
        };
        match id.inner.encrypt(plaintext, &mut rand_core::OsRng) {
            Ok(ct) => write_out(&ct, buf, cap, out_len),
            Err(e) => {
                set_last_error(format!("{e:?}"));
                LEV_ERR_CRYPTO
            }
        }
    })
}

/// Decrypt `ciphertext` with the identity's X25519 private key. Writes the
/// plaintext read(2) style (NULL `buf` queries the length). `LEV_ERR_CRYPTO` if
/// the identity has no private key or the ciphertext fails to authenticate.
#[no_mangle]
pub unsafe extern "C" fn lev_identity_decrypt(
    id: *const lev_identity_t,
    ciphertext: *const u8,
    ciphertext_len: usize,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let id = match id.as_ref() {
            Some(id) => id,
            None => return LEV_ERR_NULL_PTR,
        };
        let ciphertext = match bytes(ciphertext, ciphertext_len) {
            Some(c) => c,
            None => return LEV_ERR_NULL_PTR,
        };
        match id.inner.decrypt(ciphertext) {
            Ok(pt) => write_out(&pt, buf, cap, out_len),
            Err(e) => {
                set_last_error(format!("{e:?}"));
                LEV_ERR_CRYPTO
            }
        }
    })
}
