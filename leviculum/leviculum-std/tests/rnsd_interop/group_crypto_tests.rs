//! GROUP destination shared-key crypto interoperability with Python Reticulum.
//!
//! Python-RNS `Destination.GROUP` destinations encrypt/decrypt with a
//! pre-shared symmetric key (an `RNS.Cryptography.Token`, AES-256-CBC + HMAC).
//! These tests verify that a Rust GROUP destination and a Python GROUP
//! destination holding the SAME 64-byte key exchange a message BOTH ways:
//! Python encrypts then Rust decrypts, and Rust encrypts then Python decrypts.
//!
//! The Python side drives the real GROUP branch (`Destination.load_private_key`
//! plus `Destination.encrypt`/`decrypt`) via the `group_encrypt`/`group_decrypt`
//! daemon RPCs.
//!
//! ## Running These Tests
//!
//! ```sh
//! cargo test -p leviculum-std --test rnsd_interop -- group_crypto_tests --test-threads=1
//! ```

use leviculum_core::identity::Identity;
use leviculum_core::{Destination, DestinationType, Direction};
use rand_core::OsRng;

use crate::harness::TestDaemon;

/// Build a Rust GROUP IN destination loaded with the given shared key.
fn rust_group_dest(key: &[u8]) -> Destination {
    let identity = Identity::generate(&mut OsRng);
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Group,
        "levgroup",
        &["interop"],
    )
    .expect("create GROUP destination");
    dest.load_group_key(key).expect("load GROUP key");
    dest
}

/// A fixed, non-trivial 64-byte shared key (0x00..0x3f).
fn shared_key() -> [u8; 64] {
    let mut k = [0u8; 64];
    for (i, b) in k.iter_mut().enumerate() {
        *b = i as u8;
    }
    k
}

/// Python encrypts with a GROUP key -> Rust decrypts to the same plaintext.
#[tokio::test]
async fn test_group_python_encrypt_rust_decrypt() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let key = shared_key();
    let plaintext = b"group message from python";

    let ciphertext = daemon
        .group_encrypt(&key, plaintext)
        .await
        .expect("python group_encrypt");

    // Python must not leak the plaintext into the token bytes.
    assert!(
        !ciphertext.windows(plaintext.len()).any(|w| w == plaintext),
        "plaintext leaked into Python GROUP ciphertext"
    );

    let rust_dest = rust_group_dest(&key);
    let decrypted = rust_dest
        .decrypt(&ciphertext)
        .expect("rust GROUP decrypt of python token");
    assert_eq!(&decrypted[..], plaintext);
}

/// Rust encrypts with a GROUP key -> Python decrypts to the same plaintext.
#[tokio::test]
async fn test_group_rust_encrypt_python_decrypt() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let key = shared_key();
    let plaintext = b"group message from rust";

    let rust_dest = rust_group_dest(&key);
    let ciphertext = rust_dest
        .encrypt(plaintext, None, &mut OsRng)
        .expect("rust GROUP encrypt");

    let decrypted = daemon
        .group_decrypt(&key, &ciphertext)
        .await
        .expect("python group_decrypt of rust token");
    assert_eq!(&decrypted[..], plaintext);
}

/// Full round-trip in both directions with a Rust-generated key, plus a
/// wrong-key negative check on the Python side.
#[tokio::test]
async fn test_group_bidirectional_and_wrong_key() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Rust generates the shared key (matching Python create_keys: 64 bytes).
    let mut sender = {
        let identity = Identity::generate(&mut OsRng);
        Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Group,
            "levgroup",
            &["interop"],
        )
        .unwrap()
    };
    sender.create_group_key(&mut OsRng).unwrap();
    let key = *sender.group_key().unwrap();
    assert_eq!(key.len(), 64);

    let plaintext = b"bidirectional group payload";

    // Rust -> Python
    let ct_rust = sender.encrypt(plaintext, None, &mut OsRng).unwrap();
    let pt_py = daemon
        .group_decrypt(&key, &ct_rust)
        .await
        .expect("python decrypts rust token");
    assert_eq!(&pt_py[..], plaintext);

    // Python -> Rust
    let ct_py = daemon
        .group_encrypt(&key, plaintext)
        .await
        .expect("python encrypts");
    let receiver = rust_group_dest(&key);
    let pt_rust = receiver
        .decrypt(&ct_py)
        .expect("rust decrypts python token");
    assert_eq!(&pt_rust[..], plaintext);

    // Wrong key: Python must fail to decrypt a Rust token under a different key.
    let mut wrong_key = key;
    wrong_key[0] ^= 0xff;
    let result = daemon.group_decrypt(&wrong_key, &ct_rust).await;
    assert!(
        result.is_err(),
        "python must reject a GROUP token under the wrong key"
    );
}
