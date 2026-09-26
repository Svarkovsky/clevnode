//! Loader for the periculum test vector corpus.
//!
//! The corpus format is documented in periculum's `vectors/README.md`: one
//! JSON file per category, each holding a `schema` version, the `generator`
//! that produced it (the Python reference implementation) and a list of
//! vectors whose byte fields are lowercase hex strings.
//!
//! # Where the corpus lives
//!
//! This crate verifies `leviculum-core`, so it lives here; the corpus it
//! reads is periculum's and stays in periculum. The two are found by
//! `PERICULUM_VECTORS_DIR`, which defaults to a sibling `periculum/vectors`
//! checkout — the layout `README.md` describes. Nothing in this workspace
//! *requires* the corpus: when it is absent the tests say so by name and
//! skip, because "the corpus was not here" and "the vectors disagree" are
//! different answers and must never be printed as the same one.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Deserialize;

pub const SCHEMA_VERSION: u32 = 1;

/// One corpus file. `T` is the per-category vector type.
#[derive(Debug, Deserialize)]
pub struct Corpus<T> {
    pub schema: u32,
    pub generator: String,
    pub category: String,
    pub vectors: Vec<T>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HmacVector {
    pub name: String,
    pub description: String,
    pub key: String,
    pub data: String,
    pub mac: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AesCbcVector {
    pub name: String,
    pub description: String,
    pub key: String,
    pub iv: String,
    pub plaintext: String,
    pub padded_plaintext: String,
    pub ciphertext: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenVector {
    pub name: String,
    pub description: String,
    /// "roundtrip" (encrypt and decrypt) or "decrypt" (decrypt only).
    pub operation: String,
    pub key: String,
    pub iv: String,
    pub token: String,
    /// Present for roundtrip vectors.
    pub plaintext: Option<String>,
    /// Present for decrypt vectors.
    pub padded_plaintext: Option<String>,
    pub expected_plaintext: Option<String>,
    pub padding_variant: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationVector {
    pub name: String,
    pub description: String,
    pub app_name: String,
    pub aspects: Vec<String>,
    pub destination_hash: String,
    /// Absent for PLAIN (identity-less) destinations.
    pub identity_private_key: Option<String>,
    pub identity_public_key: Option<String>,
    pub identity_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PacketVector {
    pub name: String,
    pub description: String,
    pub raw: String,
    pub packet_hash: String,
}

/// The environment variable naming the corpus directory.
pub const VECTORS_DIR_ENV: &str = "PERICULUM_VECTORS_DIR";

/// Path of the corpus directory: `$PERICULUM_VECTORS_DIR`, else a sibling
/// `periculum/vectors` checkout next to this workspace.
///
/// The path is returned whether or not it exists; use [`corpus_present`] to
/// ask that question.
pub fn vectors_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(VECTORS_DIR_ENV) {
        return PathBuf::from(dir);
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../periculum/vectors")
}

/// Whether the corpus is reachable. `false` is a legitimate state — the
/// corpus lives in a different repository — and callers report it by name
/// rather than reading it as an empty corpus.
pub fn corpus_present() -> bool {
    vectors_dir().is_dir()
}

/// Print why a test is not running and return `true`, or return `false` when
/// the corpus is there and the test must proceed.
///
/// Loud on purpose: a skipped vector test that looks like a passing one is
/// the exact conflation this corpus exists to prevent.
pub fn skip_without_corpus(test: &str) -> bool {
    if corpus_present() {
        return false;
    }
    eprintln!(
        "SKIPPED {test}: no vector corpus at {}. The corpus lives in the \
         periculum repository; clone it beside this one or point {} at its \
         `vectors/` directory.",
        vectors_dir().display(),
        VECTORS_DIR_ENV
    );
    true
}

/// Load and validate one corpus file by file name, e.g. `"token.json"`.
///
/// Panics with context on I/O, JSON or schema mismatch: the corpus is test
/// input, a malformed file must fail loudly.
pub fn load<T: DeserializeOwned>(file_name: &str) -> Corpus<T> {
    let path = vectors_dir().join(file_name);
    let data = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read corpus file {}: {e}", path.display()));
    let corpus: Corpus<T> = serde_json::from_str(&data)
        .unwrap_or_else(|e| panic!("cannot parse corpus file {}: {e}", path.display()));
    assert_eq!(
        corpus.schema,
        SCHEMA_VERSION,
        "unsupported schema version in {}",
        path.display()
    );
    assert!(
        !corpus.vectors.is_empty(),
        "corpus file {} contains no vectors",
        path.display()
    );
    corpus
}
