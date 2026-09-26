//! Function-level unit and unhappy-path tests for the C API.
//!
//! These call the `lev_*` extern functions directly (the same ABI surface a C
//! caller hits) and need no network, so they run on any target via the rlib.

mod support;

use std::collections::HashSet;
use std::ffi::CStr;
use std::ptr;

use leviculum::*;
use support::{cstr, last_error, read2, Identity};

#[test]
fn version() {
    unsafe {
        let s = lev_version_string();
        assert!(!s.is_null());
        assert!(!CStr::from_ptr(s).to_str().unwrap().is_empty());
        assert_ne!(lev_version_number(), 0);
    }
}

#[test]
fn strerror_distinct_and_unknown() {
    unsafe {
        let msg = |c| CStr::from_ptr(lev_strerror(c)).to_str().unwrap();
        assert_eq!(msg(LEV_OK), "success");
        assert_eq!(msg(LEV_ERR_NULL_PTR), "null pointer");
        assert_eq!(msg(-9999), "unknown error");
        let codes = [
            LEV_OK,
            LEV_ERR_NULL_PTR,
            LEV_ERR_INVALID_ARG,
            LEV_ERR_BUFFER_TOO_SMALL,
            LEV_ERR_NOT_RUNNING,
            LEV_ERR_NO_PATH,
            LEV_ERR_LINK,
            LEV_ERR_TIMEOUT,
            LEV_ERR_AGAIN,
            LEV_ERR_UNKNOWN_DEST,
            LEV_ERR_PANIC,
        ];
        let msgs: HashSet<&str> = codes.iter().map(|&c| msg(c)).collect();
        assert_eq!(msgs.len(), codes.len(), "each code has a distinct message");
    }
}

#[test]
fn hex_roundtrip_and_errors() {
    unsafe {
        let bytes = [0x00u8, 0x1f, 0xab, 0xff];
        let mut need = 0usize;
        // Size query.
        assert_eq!(
            lev_hex_encode(bytes.as_ptr(), 4, ptr::null_mut(), 0, &mut need),
            LEV_ERR_BUFFER_TOO_SMALL
        );
        assert_eq!(need, 8);

        let mut hex = [0u8; 8];
        let mut l = 8usize;
        assert_eq!(
            lev_hex_encode(bytes.as_ptr(), 4, hex.as_mut_ptr(), 8, &mut l),
            LEV_OK
        );
        assert_eq!(&hex, b"001fabff");

        let mut out = [0u8; 4];
        let mut ol = 4usize;
        assert_eq!(
            lev_hex_decode(hex.as_ptr(), 8, out.as_mut_ptr(), 4, &mut ol),
            LEV_OK
        );
        assert_eq!(out, bytes);

        // Odd length and bad digit.
        assert_eq!(
            lev_hex_decode(b"abc".as_ptr(), 3, out.as_mut_ptr(), 4, &mut ol),
            LEV_ERR_INVALID_ARG
        );
        assert_eq!(
            lev_hex_decode(b"zz".as_ptr(), 2, out.as_mut_ptr(), 4, &mut ol),
            LEV_ERR_INVALID_ARG
        );
        // NULL out_len.
        assert_eq!(
            lev_hex_encode(bytes.as_ptr(), 4, hex.as_mut_ptr(), 8, ptr::null_mut()),
            LEV_ERR_NULL_PTR
        );
    }
}

// A `len` whose `len * 2` overflows usize must be rejected up front
// (LEV_ERR_INVALID_ARG), before any allocation or out-of-bounds write. The
// overflow path is reachable without a real huge buffer: a size query (NULL
// buf) hits the size computation first. `data` is a valid 1-byte pointer so the
// only thing that can stop the call is the checked multiply.
#[test]
fn hex_encode_length_overflow_is_rejected() {
    unsafe {
        let one = [0u8; 1];
        let mut need = 0usize;
        let huge = usize::MAX / 2 + 1; // huge * 2 overflows usize
        assert_eq!(
            lev_hex_encode(one.as_ptr(), huge, ptr::null_mut(), 0, &mut need),
            LEV_ERR_INVALID_ARG
        );
    }
}

#[test]
fn identity_keys_roundtrip() {
    unsafe {
        let id = Identity::generate();
        assert_eq!(lev_identity_has_private_keys(id.0), 1);

        let prv = read2(|b, c, l| lev_identity_private_key(id.0, b, c, l)).unwrap();
        assert_eq!(prv.len(), 64);
        let id2 = lev_identity_from_private_key(prv.as_ptr(), prv.len());
        assert!(!id2.is_null());
        let id2 = Identity(id2);
        assert_eq!(id.hash(), id2.hash(), "private key round-trips the hash");

        let pubk = read2(|b, c, l| lev_identity_public_key(id.0, b, c, l)).unwrap();
        assert_eq!(pubk.len(), 64);
        let po = lev_identity_from_public_key(pubk.as_ptr(), pubk.len());
        assert!(!po.is_null());
        let po = Identity(po);
        assert_eq!(lev_identity_has_private_keys(po.0), 0);
        let mut buf = [0u8; 64];
        let mut l = 64usize;
        assert_eq!(
            lev_identity_private_key(po.0, buf.as_mut_ptr(), 64, &mut l),
            LEV_ERR_CRYPTO,
            "public-only identity has no private key"
        );

        // Wrong key length is rejected.
        assert!(lev_identity_from_private_key(prv.as_ptr(), 10).is_null());
        assert!(lev_identity_from_public_key(pubk.as_ptr(), 0).is_null());
    }
}

#[test]
fn identity_file_roundtrip() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let path = cstr(dir.path().join("id").to_str().unwrap());
        let id = Identity::generate();
        assert_eq!(lev_identity_save_file(id.0, path.as_ptr()), LEV_OK);

        let loaded = lev_identity_load_file(path.as_ptr());
        assert!(!loaded.is_null());
        let loaded = Identity(loaded);
        assert_eq!(id.hash(), loaded.hash());
        assert_eq!(lev_identity_has_private_keys(loaded.0), 1);

        let missing = cstr(dir.path().join("nope").to_str().unwrap());
        assert!(lev_identity_load_file(missing.as_ptr()).is_null());
    }
}

#[test]
fn identity_null_and_buffer_guards() {
    unsafe {
        let mut buf = [0u8; 16];
        let mut l = 16usize;
        assert_eq!(
            lev_identity_hash(ptr::null(), buf.as_mut_ptr(), 16, &mut l),
            LEV_ERR_NULL_PTR
        );
        assert_eq!(lev_identity_has_private_keys(ptr::null()), 0);

        let id = Identity::generate();
        let mut small = [0u8; 4];
        let mut need = 0usize;
        assert_eq!(
            lev_identity_public_key(id.0, small.as_mut_ptr(), 4, &mut need),
            LEV_ERR_BUFFER_TOO_SMALL
        );
        assert_eq!(need, 64);
        assert_eq!(
            lev_identity_hash(id.0, ptr::null_mut(), 0, &mut need),
            LEV_ERR_BUFFER_TOO_SMALL
        );
        assert_eq!(need, 16);
        assert_eq!(
            lev_identity_hash(id.0, buf.as_mut_ptr(), 16, ptr::null_mut()),
            LEV_ERR_NULL_PTR
        );
    }
}

#[test]
fn destination_validation() {
    unsafe {
        let id = Identity::generate();
        let app = cstr("app");
        let asp = cstr("a");
        let asp_ptrs = [asp.as_ptr()];

        // PLAIN cannot have an identity.
        assert!(lev_destination_new(
            id.0,
            LEV_DIRECTION_IN,
            LEV_DEST_PLAIN,
            app.as_ptr(),
            asp_ptrs.as_ptr(),
            1
        )
        .is_null());
        // Invalid direction / type.
        assert!(lev_destination_new(
            id.0,
            99,
            LEV_DEST_SINGLE,
            app.as_ptr(),
            asp_ptrs.as_ptr(),
            1
        )
        .is_null());
        assert!(lev_destination_new(
            id.0,
            LEV_DIRECTION_IN,
            99,
            app.as_ptr(),
            asp_ptrs.as_ptr(),
            1
        )
        .is_null());
        // NULL app_name.
        assert!(lev_destination_new(
            id.0,
            LEV_DIRECTION_IN,
            LEV_DEST_SINGLE,
            ptr::null(),
            asp_ptrs.as_ptr(),
            1
        )
        .is_null());

        // Valid SINGLE; hash readable; freeable.
        let d = lev_destination_new(
            id.0,
            LEV_DIRECTION_IN,
            LEV_DEST_SINGLE,
            app.as_ptr(),
            asp_ptrs.as_ptr(),
            1,
        );
        assert!(!d.is_null());
        let mut h = [0u8; 16];
        let mut l = 16usize;
        assert_eq!(lev_destination_hash(d, h.as_mut_ptr(), 16, &mut l), LEV_OK);
        lev_destination_free(d);
    }
}

#[test]
fn destination_set_proof_strategy_validates() {
    unsafe {
        let id = Identity::generate();
        let app = cstr("app");
        let asp = cstr("p");
        let asp_ptrs = [asp.as_ptr()];
        let dest = lev_destination_new(
            id.0,
            LEV_DIRECTION_IN,
            LEV_DEST_SINGLE,
            app.as_ptr(),
            asp_ptrs.as_ptr(),
            1,
        );
        assert!(!dest.is_null());
        for s in [LEV_PROOF_NONE, LEV_PROOF_APP, LEV_PROOF_ALL] {
            assert_eq!(lev_destination_set_proof_strategy(dest, s), LEV_OK);
        }
        assert_eq!(
            lev_destination_set_proof_strategy(dest, 99),
            LEV_ERR_INVALID_ARG
        );
        assert_eq!(
            lev_destination_set_proof_strategy(ptr::null_mut(), LEV_PROOF_APP),
            LEV_ERR_NULL_PTR
        );
        lev_destination_free(dest);
    }
}

#[test]
fn destination_enable_ratchets_validates() {
    unsafe {
        let id = Identity::generate();
        let app = cstr("app");
        let asp = cstr("r");
        let asp_ptrs = [asp.as_ptr()];

        // An inbound destination accepts ratchets.
        let inbound = lev_destination_new(
            id.0,
            LEV_DIRECTION_IN,
            LEV_DEST_SINGLE,
            app.as_ptr(),
            asp_ptrs.as_ptr(),
            1,
        );
        assert!(!inbound.is_null());
        assert_eq!(
            lev_destination_enable_ratchets(inbound, 1_700_000_000_000),
            LEV_OK
        );
        lev_destination_free(inbound);

        // An outbound destination cannot ratchet.
        let outbound = lev_destination_new(
            id.0,
            LEV_DIRECTION_OUT,
            LEV_DEST_SINGLE,
            app.as_ptr(),
            asp_ptrs.as_ptr(),
            1,
        );
        assert!(!outbound.is_null());
        assert_eq!(
            lev_destination_enable_ratchets(outbound, 1_700_000_000_000),
            LEV_ERR_INVALID_ARG
        );
        lev_destination_free(outbound);

        // NULL guard.
        assert_eq!(
            lev_destination_enable_ratchets(ptr::null_mut(), 0),
            LEV_ERR_NULL_PTR
        );
    }
}

// The accepts-links knob (Codeberg #21): an inbound destination defaults to
// accepting, can be switched off before registration, and the flag survives the
// register path (which forwards it to register_destination_with_links). The
// behavioural proof that accepts_links == false declines an inbound link lives
// in leviculum-core (inbound_link_rejected_when_destination_does_not_accept);
// here we cover the FFI plumbing and the consumed-handle contract.
#[test]
fn destination_accepts_links_knob() {
    unsafe {
        let id = Identity::generate();
        let app = cstr("app");
        let asp = cstr("r");
        let asp_ptrs = [asp.as_ptr()];

        let dest = lev_destination_new(
            id.0,
            LEV_DIRECTION_IN,
            LEV_DEST_SINGLE,
            app.as_ptr(),
            asp_ptrs.as_ptr(),
            1,
        );
        assert!(!dest.is_null());
        // Toggle off, then back on, both before registration.
        assert_eq!(lev_destination_set_accepts_links(dest, 0), LEV_OK);
        assert_eq!(lev_destination_set_accepts_links(dest, 1), LEV_OK);

        // NULL guard.
        assert_eq!(
            lev_destination_set_accepts_links(ptr::null_mut(), 0),
            LEV_ERR_NULL_PTR
        );

        // Build a node and register the destination with accepts-links off.
        let dir = tempfile::tempdir().unwrap();
        let b = lev_builder_new();
        let sp = cstr(dir.path().to_str().unwrap());
        assert_eq!(lev_builder_storage_path(b, sp.as_ptr()), LEV_OK);
        let node = lev_builder_build(b);
        lev_builder_free(b);
        assert!(!node.is_null(), "build failed: {}", last_error());

        assert_eq!(lev_destination_set_accepts_links(dest, 0), LEV_OK);
        assert_eq!(lev_register_destination(node, dest), LEV_OK);
        // After registration the inner destination is consumed: the knob now
        // reports the already-registered error instead of touching freed state.
        assert_eq!(
            lev_destination_set_accepts_links(dest, 1),
            LEV_ERR_INVALID_ARG
        );

        lev_destination_free(dest);
        lev_free(node);
    }
}

#[test]
fn builder_validation() {
    unsafe {
        let b = lev_builder_new();
        assert!(!b.is_null());

        let bad = cstr("not-a-host-port");
        assert_eq!(
            lev_builder_add_tcp_client(b, bad.as_ptr()),
            LEV_ERR_INVALID_ARG
        );
        assert!(!last_error().is_empty());
        assert_eq!(lev_builder_event_capacity(b, 16, 16), LEV_OK);
        assert_eq!(
            lev_builder_storage_path(b, ptr::null()),
            LEV_ERR_INVALID_ARG
        );

        let dir = tempfile::tempdir().unwrap();
        let sp = cstr(dir.path().to_str().unwrap());
        assert_eq!(lev_builder_storage_path(b, sp.as_ptr()), LEV_OK);

        let node = lev_builder_build(b);
        assert!(!node.is_null());
        // Second build fails; the empty shell is still ours to free.
        assert!(lev_builder_build(b).is_null());
        // A setter after consumption is an error, not a crash.
        assert_eq!(lev_builder_enable_transport(b, 1), LEV_ERR_INVALID_ARG);
        lev_builder_free(b);
        lev_free(node);
    }
}

#[test]
fn log_level_validation() {
    // These entry points take no pointers and are safe to call.
    assert_eq!(lev_init(), LEV_OK);
    assert_eq!(lev_log_set_level(LEV_LOG_INFO), LEV_OK);
    assert_eq!(lev_log_set_level(9999), LEV_ERR_INVALID_ARG);
    assert_eq!(lev_log_set_level(-1), LEV_ERR_INVALID_ARG);
    assert_eq!(lev_log_set_level(LEV_LOG_OFF), LEV_OK);
}

#[test]
fn phase1_builder_setters_validate_args() {
    unsafe {
        let b = lev_builder_new();
        assert_eq!(lev_builder_config_file(b, ptr::null()), LEV_ERR_INVALID_ARG);
        assert_eq!(
            lev_builder_share_instance(b, ptr::null()),
            LEV_ERR_INVALID_ARG
        );
        assert_eq!(
            lev_builder_connect_shared_instance(b, ptr::null()),
            LEV_ERR_INVALID_ARG
        );
        let name = cstr("levtest");
        assert_eq!(lev_builder_share_instance(b, name.as_ptr()), LEV_OK);
        lev_builder_free(b);
    }
}

#[test]
fn identity_crypto_sign_verify_encrypt_decrypt() {
    unsafe {
        let id = Identity::generate();
        let msg = b"the quick brown fox";

        // Sign, then verify the signature round-trips.
        let sig = read2(|b, c, l| lev_identity_sign(id.0, msg.as_ptr(), msg.len(), b, c, l))
            .expect("sign");
        assert_eq!(sig.len(), 64);
        assert_eq!(
            lev_identity_verify(id.0, msg.as_ptr(), msg.len(), sig.as_ptr(), sig.len()),
            1
        );
        // A tampered message or signature does not verify.
        let bad = b"the quick brown FOX";
        assert_eq!(
            lev_identity_verify(id.0, bad.as_ptr(), bad.len(), sig.as_ptr(), sig.len()),
            0
        );
        let mut sig2 = sig.clone();
        sig2[0] ^= 0xFF;
        assert_eq!(
            lev_identity_verify(id.0, msg.as_ptr(), msg.len(), sig2.as_ptr(), sig2.len()),
            0
        );
        // A wrong-length signature simply does not verify (0), never a panic.
        assert_eq!(
            lev_identity_verify(id.0, msg.as_ptr(), msg.len(), sig.as_ptr(), 10),
            0
        );

        // A public-only identity can verify but cannot sign.
        let pubkey = read2(|b, c, l| lev_identity_public_key(id.0, b, c, l)).expect("public key");
        let pub_only = Identity(lev_identity_from_public_key(pubkey.as_ptr(), pubkey.len()));
        assert!(!pub_only.0.is_null());
        assert_eq!(
            lev_identity_verify(pub_only.0, msg.as_ptr(), msg.len(), sig.as_ptr(), sig.len()),
            1
        );
        let mut nl = 0usize;
        assert_eq!(
            lev_identity_sign(
                pub_only.0,
                msg.as_ptr(),
                msg.len(),
                ptr::null_mut(),
                0,
                &mut nl
            ),
            LEV_ERR_CRYPTO
        );

        // Encrypt to the public key; only the private-key holder decrypts.
        let ct =
            read2(|b, c, l| lev_identity_encrypt(pub_only.0, msg.as_ptr(), msg.len(), b, c, l))
                .expect("encrypt");
        assert!(ct.len() >= 96, "ciphertext carries the scheme overhead");
        let pt = read2(|b, c, l| lev_identity_decrypt(id.0, ct.as_ptr(), ct.len(), b, c, l))
            .expect("decrypt");
        assert_eq!(pt, msg);
        assert_eq!(
            read2(|b, c, l| lev_identity_decrypt(pub_only.0, ct.as_ptr(), ct.len(), b, c, l))
                .unwrap_err(),
            LEV_ERR_CRYPTO
        );
        let junk = [0u8; 100];
        assert_eq!(
            read2(|b, c, l| lev_identity_decrypt(id.0, junk.as_ptr(), junk.len(), b, c, l))
                .unwrap_err(),
            LEV_ERR_CRYPTO
        );

        // NULL-identity guards.
        let mut l0 = 0usize;
        assert_eq!(
            lev_identity_sign(
                ptr::null(),
                msg.as_ptr(),
                msg.len(),
                ptr::null_mut(),
                0,
                &mut l0
            ),
            LEV_ERR_NULL_PTR
        );
        assert_eq!(
            lev_identity_verify(
                ptr::null(),
                msg.as_ptr(),
                msg.len(),
                sig.as_ptr(),
                sig.len()
            ),
            LEV_ERR_NULL_PTR
        );
    }
}

#[test]
fn builder_accepts_auto_and_udp_interfaces() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let b = lev_builder_new();
        let sp = cstr(dir.path().to_str().unwrap());
        assert_eq!(lev_builder_storage_path(b, sp.as_ptr()), LEV_OK);
        // The AutoInterface setter is accepted (it needs IPv6 multicast to come
        // up, so only the configuration step is exercised here).
        assert_eq!(lev_builder_add_auto_interface(b), LEV_OK);
        let listen = cstr("127.0.0.1:0");
        let forward = cstr("127.0.0.1:0");
        assert_eq!(
            lev_builder_add_udp(b, listen.as_ptr(), forward.as_ptr()),
            LEV_OK
        );
        let node = lev_builder_build(b);
        lev_builder_free(b);
        assert!(!node.is_null(), "build with auto+udp: {}", last_error());
        lev_free(node);
    }
}

#[test]
fn phase2_radio_setters_validate_args() {
    unsafe {
        let b = lev_builder_new();
        // NULL device path / parity is rejected; the builder stays usable.
        assert_eq!(
            lev_builder_add_rnode(b, ptr::null(), 867_200_000, 125_000, 8, 5, 0),
            LEV_ERR_INVALID_ARG
        );
        assert_eq!(
            lev_builder_add_serial(b, ptr::null(), 115_200, 8, cstr("N").as_ptr(), 1),
            LEV_ERR_INVALID_ARG
        );
        let dev = cstr("/dev/null");
        assert_eq!(
            lev_builder_add_serial(b, dev.as_ptr(), 115_200, 8, ptr::null(), 1),
            LEV_ERR_INVALID_ARG
        );
        // Valid argument shapes are accepted by the setters (no device opened
        // until build/start).
        assert_eq!(
            lev_builder_add_rnode(b, dev.as_ptr(), 867_200_000, 125_000, 8, 5, 0),
            LEV_OK
        );
        assert_eq!(
            lev_builder_add_serial(b, dev.as_ptr(), 115_200, 8, cstr("N").as_ptr(), 1),
            LEV_OK
        );
        lev_builder_free(b);
    }
}

#[test]
fn transport_stats_guards_and_bare_node() {
    unsafe {
        // NULL node is rejected.
        let mut v = 0u64;
        assert_eq!(
            lev_transport_stats(
                ptr::null(),
                &mut v,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            LEV_ERR_NULL_PTR
        );

        let dir = tempfile::tempdir().unwrap();
        let b = lev_builder_new();
        let sp = cstr(dir.path().to_str().unwrap());
        assert_eq!(lev_builder_storage_path(b, sp.as_ptr()), LEV_OK);
        assert_eq!(lev_builder_enable_transport(b, 0), LEV_OK);
        let node = lev_builder_build(b);
        lev_builder_free(b);
        assert!(!node.is_null());
        assert_eq!(lev_start(node), LEV_OK);

        // All out-pointers NULL is allowed (a no-op query).
        assert_eq!(
            lev_transport_stats(
                node,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            LEV_OK
        );

        // A fresh node has no paths and has not dropped packets.
        let mut paths = 99u64;
        let mut dropped = 99u64;
        assert_eq!(
            lev_transport_stats(
                node,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut dropped,
                &mut paths,
            ),
            LEV_OK
        );
        assert_eq!(paths, 0);
        assert_eq!(dropped, 0);

        lev_stop(node);
        lev_free(node);
    }
}

#[test]
fn path_table_snapshot_guards_and_empty() {
    unsafe {
        // NULL node yields a NULL snapshot; NULL/empty handles are safe.
        assert!(lev_path_table_snapshot(ptr::null()).is_null());
        assert_eq!(lev_path_table_count(ptr::null()), 0);
        lev_path_table_free(ptr::null_mut()); // no-op

        let dir = tempfile::tempdir().unwrap();
        let b = lev_builder_new();
        let sp = cstr(dir.path().to_str().unwrap());
        assert_eq!(lev_builder_storage_path(b, sp.as_ptr()), LEV_OK);
        assert_eq!(lev_builder_enable_transport(b, 0), LEV_OK);
        let node = lev_builder_build(b);
        lev_builder_free(b);
        assert!(!node.is_null());
        assert_eq!(lev_start(node), LEV_OK);

        // A fresh node has an empty path table.
        let table = lev_path_table_snapshot(node);
        assert!(!table.is_null());
        assert_eq!(lev_path_table_count(table), 0);
        // Any index is out of range on an empty snapshot.
        assert_eq!(
            lev_path_table_entry(
                table,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            LEV_ERR_INVALID_ARG
        );
        lev_path_table_free(table);

        lev_stop(node);
        lev_free(node);
    }
}

#[test]
fn interface_stats_snapshot_guards_and_bare_node() {
    unsafe {
        assert!(lev_interface_stats_snapshot(ptr::null()).is_null());
        assert_eq!(lev_interface_stats_count(ptr::null()), 0);
        lev_interface_stats_free(ptr::null_mut()); // no-op

        let dir = tempfile::tempdir().unwrap();
        let b = lev_builder_new();
        let sp = cstr(dir.path().to_str().unwrap());
        assert_eq!(lev_builder_storage_path(b, sp.as_ptr()), LEV_OK);
        assert_eq!(lev_builder_enable_transport(b, 0), LEV_OK);
        let node = lev_builder_build(b);
        lev_builder_free(b);
        assert!(!node.is_null());
        assert_eq!(lev_start(node), LEV_OK);

        // A node with no interfaces has an empty snapshot; any index is out of
        // range and the scalar reader rejects it.
        let table = lev_interface_stats_snapshot(node);
        assert!(!table.is_null());
        assert_eq!(lev_interface_stats_count(table), 0);
        let mut online = 0i32;
        assert_eq!(
            lev_interface_stats_entry(
                table,
                0,
                &mut online,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            LEV_ERR_INVALID_ARG
        );
        let mut nl = 0usize;
        assert_eq!(
            lev_interface_stats_name(table, 0, ptr::null_mut(), 0, &mut nl),
            LEV_ERR_INVALID_ARG
        );
        lev_interface_stats_free(table);

        lev_stop(node);
        lev_free(node);
    }
}

#[test]
fn config_file_brings_up_a_node() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        // listen_port 0: the kernel assigns the port at bind, so the test
        // needs no free-port probe (Codeberg #221). Nothing dials this
        // server; the test only asserts the config brings a node up.
        let cfg = "[reticulum]\n  enable_transport = no\n\n[interfaces]\n  \
             [[Test TCP Server]]\n    type = TCPServerInterface\n    enabled = yes\n    \
             listen_ip = 127.0.0.1\n    listen_port = 0\n    mode = gateway\n"
            .to_string();
        let cfg_path = dir.path().join("config");
        std::fs::write(&cfg_path, cfg).unwrap();

        let b = lev_builder_new();
        let sp = cstr(dir.path().to_str().unwrap());
        assert_eq!(lev_builder_storage_path(b, sp.as_ptr()), LEV_OK);
        let cf = cstr(cfg_path.to_str().unwrap());
        assert_eq!(lev_builder_config_file(b, cf.as_ptr()), LEV_OK);

        let node = lev_builder_build(b);
        lev_builder_free(b);
        assert!(!node.is_null(), "build with config_file: {}", last_error());
        assert_eq!(lev_start(node), LEV_OK, "start: {}", last_error());
        assert_eq!(lev_is_running(node), 1);
        assert_eq!(lev_stop(node), LEV_OK);
        lev_free(node);
    }
}

#[test]
fn node_null_guards_and_free_null() {
    unsafe {
        assert_eq!(lev_start(ptr::null_mut()), LEV_ERR_NULL_PTR);
        assert_eq!(lev_stop(ptr::null_mut()), LEV_ERR_NULL_PTR);
        assert_eq!(lev_is_running(ptr::null()), 0);
        assert!(lev_event_fd(ptr::null()) < 0);

        let dest = [0u8; 16];
        assert_eq!(lev_has_path(ptr::null(), dest.as_ptr()), LEV_ERR_NULL_PTR);
        let mut link: *mut lev_link_t = ptr::null_mut();
        assert_eq!(
            lev_connect(ptr::null(), dest.as_ptr(), 100, &mut link),
            LEV_ERR_NULL_PTR
        );
        let mut ev: *mut lev_event_t = ptr::null_mut();
        assert_eq!(lev_next_event(ptr::null_mut(), &mut ev), LEV_ERR_NULL_PTR);

        // Every free is a no-op on NULL.
        lev_free(ptr::null_mut());
        lev_builder_free(ptr::null_mut());
        lev_identity_free(ptr::null_mut());
        lev_destination_free(ptr::null_mut());
        lev_link_free(ptr::null_mut());
        lev_event_free(ptr::null_mut());
    }
}
