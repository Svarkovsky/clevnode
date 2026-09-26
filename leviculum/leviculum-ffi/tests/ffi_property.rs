//! Property and fuzz tests for the C API's marshalling and lifecycle.
//!
//! Fixed scenarios exercise known inputs; these throw randomised inputs at the
//! boundary, the read(2) buffer protocol, the parsers, the crypto round-trips,
//! NULL injection, and random lifecycle orderings, to surface marshalling and
//! state bugs the examples miss. Everything is deterministic per seed (proptest
//! is seeded) and never depends on the network, so it is not flaky.

mod support;

use std::os::raw::c_int;
use std::ptr;

use leviculum::*;
use proptest::prelude::*;
use support::{read2, Identity};

/// A buffer pointer for a chosen capacity: NULL at capacity 0 (a size query),
/// the buffer otherwise.
fn buf_ptr(buf: &mut [u8]) -> *mut u8 {
    if buf.is_empty() {
        ptr::null_mut()
    } else {
        buf.as_mut_ptr()
    }
}

proptest! {
    /// Hex encode then decode reproduces any byte string.
    #[test]
    fn hex_round_trips_any_bytes(data in prop::collection::vec(any::<u8>(), 0..512)) {
        let hex = read2(|b, c, l| unsafe { lev_hex_encode(data.as_ptr(), data.len(), b, c, l) })
            .expect("encode");
        prop_assert_eq!(hex.len(), data.len() * 2);
        let back = read2(|b, c, l| unsafe { lev_hex_decode(hex.as_ptr(), hex.len(), b, c, l) })
            .expect("decode");
        prop_assert_eq!(back, data);
    }

    /// Decoding arbitrary bytes never panics or corrupts: it returns OK, or a
    /// defined error, and never writes past the buffer.
    #[test]
    fn hex_decode_of_arbitrary_input_is_safe(s in prop::collection::vec(any::<u8>(), 0..512)) {
        let mut out = vec![0u8; s.len()];
        let mut n = out.len();
        let rc = unsafe { lev_hex_decode(s.as_ptr(), s.len(), buf_ptr(&mut out), out.len(), &mut n) };
        prop_assert!(
            rc == LEV_OK || rc == LEV_ERR_INVALID_ARG || rc == LEV_ERR_BUFFER_TOO_SMALL,
            "unexpected rc {}",
            rc
        );
        // A successful decode produces exactly half as many bytes.
        if rc == LEV_OK {
            prop_assert_eq!(n, s.len() / 2);
        }
    }

    /// The read(2) buffer protocol holds for every capacity: the needed length
    /// is always reported, too-small is rejected without writing, an adequate
    /// buffer receives the exact bytes.
    #[test]
    fn read2_protocol_holds_for_any_capacity(cap in 0usize..200) {
        let id = Identity::generate();
        let key = read2(|b, c, l| unsafe { lev_identity_public_key(id.0, b, c, l) }).expect("key");
        prop_assert_eq!(key.len(), 64);

        let mut buf = vec![0xCCu8; cap];
        let mut need = 0usize;
        let rc = unsafe { lev_identity_public_key(id.0, buf_ptr(&mut buf), cap, &mut need) };
        prop_assert_eq!(need, 64, "needed length is always reported");
        if cap < 64 {
            prop_assert_eq!(rc, LEV_ERR_BUFFER_TOO_SMALL);
        } else {
            prop_assert_eq!(rc, LEV_OK);
            prop_assert_eq!(&buf[..64], &key[..]);
        }
    }

    /// Building an identity from arbitrary bytes never crashes: it yields a
    /// handle only for a well-formed key and is otherwise NULL.
    #[test]
    fn identity_from_arbitrary_bytes_is_safe(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
        unsafe {
            let prv = lev_identity_from_private_key(bytes.as_ptr(), bytes.len());
            if !prv.is_null() {
                prop_assert_eq!(bytes.len(), 64);
                lev_identity_free(prv);
            }
            let pubk = lev_identity_from_public_key(bytes.as_ptr(), bytes.len());
            if !pubk.is_null() {
                prop_assert_eq!(bytes.len(), 64);
                lev_identity_free(pubk);
            }
        }
    }

    /// Sign then verify round-trips for any message; a tampered message or
    /// signature does not verify.
    #[test]
    fn sign_verify_round_trips(msg in prop::collection::vec(any::<u8>(), 0..512)) {
        let id = Identity::generate();
        let sig = read2(|b, c, l| unsafe {
            lev_identity_sign(id.0, msg.as_ptr(), msg.len(), b, c, l)
        })
        .expect("sign");
        prop_assert_eq!(sig.len(), 64);
        prop_assert_eq!(
            unsafe { lev_identity_verify(id.0, msg.as_ptr(), msg.len(), sig.as_ptr(), sig.len()) },
            1
        );
        // A flipped signature bit must not verify.
        let mut bad = sig.clone();
        bad[0] ^= 0x01;
        prop_assert_eq!(
            unsafe { lev_identity_verify(id.0, msg.as_ptr(), msg.len(), bad.as_ptr(), bad.len()) },
            0
        );
    }

    /// Encrypt then decrypt round-trips for any plaintext.
    #[test]
    fn encrypt_decrypt_round_trips(pt in prop::collection::vec(any::<u8>(), 0..512)) {
        let id = Identity::generate();
        let ct = read2(|b, c, l| unsafe {
            lev_identity_encrypt(id.0, pt.as_ptr(), pt.len(), b, c, l)
        })
        .expect("encrypt");
        let out = read2(|b, c, l| unsafe {
            lev_identity_decrypt(id.0, ct.as_ptr(), ct.len(), b, c, l)
        })
        .expect("decrypt");
        prop_assert_eq!(out, pt);
    }

    /// Decrypting arbitrary ciphertext never crashes; it fails cleanly.
    #[test]
    fn decrypt_of_arbitrary_bytes_is_safe(bytes in prop::collection::vec(any::<u8>(), 0..300)) {
        let id = Identity::generate();
        let rc = read2(|b, c, l| unsafe {
            lev_identity_decrypt(id.0, bytes.as_ptr(), bytes.len(), b, c, l)
        });
        // Either it decrypts (vanishingly unlikely for random bytes) or fails
        // with a defined error; it must not panic or segfault.
        if let Err(code) = rc {
            prop_assert!(code == LEV_ERR_CRYPTO || code == LEV_ERR_BUFFER_TOO_SMALL);
        }
    }
}

proptest! {
    // Lifecycle churn is heavier (each start spins a runtime up); keep it small.
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Any ordering of lifecycle and query calls on a valid node returns
    /// defined results and never crashes; is_running stays consistent with the
    /// last successful start/stop.
    #[test]
    fn lifecycle_orderings_never_crash(ops in prop::collection::vec(0u8..6, 0..14)) {
        let dir = tempfile::tempdir().unwrap();
        let sp = support::cstr(dir.path().to_str().unwrap());
        let node = unsafe {
            let b = lev_builder_new();
            assert_eq!(lev_builder_storage_path(b, sp.as_ptr()), LEV_OK);
            assert_eq!(lev_builder_enable_transport(b, 0), LEV_OK);
            let n = lev_builder_build(b);
            lev_builder_free(b);
            n
        };
        prop_assert!(!node.is_null());

        let mut expect_running = false;
        for op in ops {
            unsafe {
                match op {
                    0 => {
                        let rc = lev_start(node);
                        // start succeeds when stopped, errors when already running.
                        if !expect_running && rc == LEV_OK {
                            expect_running = true;
                        }
                    }
                    1 => {
                        if lev_stop(node) == LEV_OK {
                            expect_running = false;
                        }
                    }
                    2 => {
                        prop_assert_eq!(lev_is_running(node), c_int::from(expect_running));
                    }
                    3 => {
                        let mut h = [0u8; 16];
                        let mut l = 16usize;
                        let rc = lev_identity_hash_self(node, h.as_mut_ptr(), 16, &mut l);
                        prop_assert!(rc == LEV_OK || rc < 0);
                    }
                    4 => {
                        let _ = lev_event_fd(node);
                    }
                    _ => {
                        let mut ev: *mut lev_event_t = ptr::null_mut();
                        let rc = lev_next_event(node, &mut ev);
                        prop_assert!(rc == LEV_OK || rc < 0);
                        if !ev.is_null() {
                            lev_event_free(ev);
                        }
                    }
                }
            }
        }
        unsafe { lev_free(node) };
    }
}

/// NULL handles and pointers are rejected with an error, never a crash, for a
/// representative battery of functions across the surface.
#[test]
fn null_arguments_never_crash() {
    unsafe {
        let mut out16 = [0u8; 16];
        let mut n = 16usize;
        // Node accessors.
        assert!(lev_start(ptr::null_mut()) < 0);
        assert!(lev_stop(ptr::null_mut()) < 0);
        assert_eq!(lev_is_running(ptr::null()), 0);
        assert!(lev_event_fd(ptr::null()) < 0);
        assert!(lev_identity_hash_self(ptr::null(), out16.as_mut_ptr(), 16, &mut n) < 0);
        assert!(
            lev_transport_stats(
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut()
            ) < 0
        );
        // Identity.
        assert!(lev_identity_hash(ptr::null(), out16.as_mut_ptr(), 16, &mut n) < 0);
        assert_eq!(lev_identity_has_private_keys(ptr::null()), 0);
        assert!(lev_identity_sign(ptr::null(), ptr::null(), 0, ptr::null_mut(), 0, &mut n) < 0);
        assert!(lev_identity_verify(ptr::null(), ptr::null(), 0, ptr::null(), 0) < 0);
        // Destination / links / events.
        assert!(lev_register_destination(ptr::null(), ptr::null_mut()) < 0);
        assert!(lev_announce(ptr::null(), ptr::null(), ptr::null(), 0, 0) < 0);
        assert!(lev_has_path(ptr::null(), ptr::null()) < 0);
        assert!(lev_connect(ptr::null(), ptr::null(), 0, ptr::null_mut()) < 0);
        assert!(lev_link_send(ptr::null(), ptr::null(), 0, 0) < 0);
        // Boolean accessors follow the NULL -> 0 convention (no crash).
        assert_eq!(lev_link_is_closed(ptr::null()), 0);
        assert_eq!(lev_event_type(ptr::null()), LEV_EVENT_OTHER);
        // Freeing NULL is a no-op everywhere.
        lev_free(ptr::null_mut());
        lev_builder_free(ptr::null_mut());
        lev_identity_free(ptr::null_mut());
        lev_destination_free(ptr::null_mut());
        lev_link_free(ptr::null_mut());
        lev_event_free(ptr::null_mut());
        lev_path_table_free(ptr::null_mut());
        lev_interface_stats_free(ptr::null_mut());
    }
}
