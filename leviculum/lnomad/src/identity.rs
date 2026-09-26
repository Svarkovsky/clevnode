//! lnomad's own persistent Reticulum identity.
//!
//! The browser identifies itself to a node (see [`crate::identify`]) with an
//! identity of its own, distinct from the shared instance's transport identity.
//! It lives in the lnomad app config dir next to the bookmarks (see
//! [`app_config_dir`]) as a checksummed 72-byte "RTIC" record, the same on-disk
//! format the firmware identity stores use
//! ([`leviculum_core::identity_store`]).
//!
//! A missing or corrupt file is not an error: a fresh identity is minted and
//! written in its place, matching the reference browser's first-run behaviour.

use std::path::{Path, PathBuf};

use leviculum_core::identity_store::{decode_identity, encode_identity};
use leviculum_std::Identity;

/// The lnomad app config dir: `${XDG_CONFIG_HOME:-~/.config}/lnomad`. `None`
/// when neither `XDG_CONFIG_HOME` nor `HOME` is set, in which case the browser
/// runs without persistence (same convention as the bookmarks store).
pub fn app_config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("lnomad"))
}

/// The default identity-file path: `${XDG_CONFIG_HOME:-~/.config}/lnomad/
/// identity`.
pub fn default_path() -> Option<PathBuf> {
    Some(app_config_dir()?.join("identity"))
}

/// Load the identity stored at `path`, or mint a fresh one and persist it when
/// the file is missing or corrupt. Only a filesystem error writing the fresh
/// identity is surfaced; a decode failure never is, so a hand-mangled file
/// cannot block startup.
pub fn load_or_create(path: &Path) -> std::io::Result<Identity> {
    if let Ok(bytes) = std::fs::read(path) {
        if let Some(identity) = decode_identity(&bytes) {
            return Ok(identity);
        }
    }
    let identity = leviculum_std::generate_identity();
    let encoded = encode_identity(&identity)
        .ok_or_else(|| std::io::Error::other("generated identity has no private key"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, encoded)?;
    Ok(identity)
}

/// A fingerprint (identity hash) as lowercase hex, for display and config keys.
pub fn fingerprint_hex(fingerprint: &[u8; 16]) -> String {
    let mut s = String::with_capacity(fingerprint.len() * 2);
    for byte in fingerprint {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_mints_and_persists_a_fresh_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/identity");
        assert!(!path.exists());

        let minted = load_or_create(&path).unwrap();
        assert!(path.exists(), "fresh identity must be written to disk");

        // A second load returns the persisted identity, not a new one.
        let reloaded = load_or_create(&path).unwrap();
        assert_eq!(reloaded.hash(), minted.hash());
    }

    #[test]
    fn corrupt_file_is_replaced_by_a_fresh_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity");
        std::fs::write(&path, b"not an identity record").unwrap();

        let minted = load_or_create(&path).unwrap();
        // The corrupt file was overwritten: the next load is stable.
        let reloaded = load_or_create(&path).unwrap();
        assert_eq!(reloaded.hash(), minted.hash());
    }

    #[test]
    fn truncated_record_is_treated_as_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity");
        let full = encode_identity(&leviculum_std::generate_identity()).unwrap();
        std::fs::write(&path, &full[..full.len() / 2]).unwrap();

        let minted = load_or_create(&path).unwrap();
        let reloaded = load_or_create(&path).unwrap();
        assert_eq!(reloaded.hash(), minted.hash());
    }

    #[test]
    fn fingerprint_hex_is_lowercase_and_full_width() {
        let mut fp = [0u8; 16];
        fp[0] = 0xAB;
        fp[15] = 0x01;
        let hex = fingerprint_hex(&fp);
        assert_eq!(hex.len(), 32);
        assert!(hex.starts_with("ab"));
        assert!(hex.ends_with("01"));
    }
}
