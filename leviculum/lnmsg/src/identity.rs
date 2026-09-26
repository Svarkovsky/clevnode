//! `lnmsg`'s own persistent Reticulum identity, and where its state lives.
//!
//! # Why this is not `lnomad`'s `load_or_create`
//!
//! `lnomad` mints a fresh identity whenever the stored one fails to decode
//! (`lnomad/src/identity.rs:39-53`), and for a browser that is right: its
//! identity is disposable. A messenger's identity **is** the user's address.
//! Replacing it silently would change what every correspondent has written
//! down, with no warning and no way back — the architecture record calls that
//! default out by name and says it must be inverted
//! (`docs/src/concepts/lnmsg-architecture.md:110-115`). So a missing file is a
//! first run and mints; an unreadable or corrupt file is an error that stops
//! the program and says which file to look at.

use std::path::{Path, PathBuf};

use leviculum_core::identity_store::{decode_identity, encode_identity};
use leviculum_std::Identity;

/// Environment override for the state directory, mirroring the `LXMF_STORAGE`
/// precedent in `leviculum-lxmf-node`. Set by tests to get an isolated address
/// per run; a user has no reason to touch it.
pub const HOME_ENV: &str = "LNMSG_HOME";

/// Why the identity could not be established.
#[derive(Debug)]
pub enum IdentityError {
    /// Neither `XDG_CONFIG_HOME` nor `HOME` nor [`HOME_ENV`] is set, so there
    /// is nowhere to keep an address.
    NoHome,
    /// The file exists and is not an identity record. Deliberately fatal.
    Corrupt(PathBuf),
    /// A filesystem error, with the path it happened on.
    Io(PathBuf, std::io::Error),
    /// A freshly generated identity had no private key to store.
    NotStorable,
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoHome => write!(
                f,
                "no home directory to keep an address in; set {HOME_ENV} or HOME"
            ),
            Self::Corrupt(path) => write!(
                f,
                "{} is not a readable identity record.\n  \
                 This file is your LXMF address: it is not replaced automatically, \
                 because a new one would silently change the address your \
                 correspondents have.\n  \
                 Restore it from a backup, or move it aside to start over with a \
                 new address.",
                path.display()
            ),
            Self::Io(path, error) => write!(f, "{}: {error}", path.display()),
            Self::NotStorable => write!(f, "the generated identity has no private key to store"),
        }
    }
}

impl std::error::Error for IdentityError {}

/// `${LNMSG_HOME:-${XDG_CONFIG_HOME:-~/.config}/lnmsg}`: identity and node
/// storage, in one directory, the way `lnomad` keeps its own.
pub fn home_dir() -> Result<PathBuf, IdentityError> {
    if let Some(explicit) = std::env::var_os(HOME_ENV)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
    {
        return Ok(explicit);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or(IdentityError::NoHome)?;
    Ok(base.join("lnmsg"))
}

/// Load the identity at `path`, minting and persisting one only if the file is
/// not there at all. See the module header for why corruption is fatal.
pub fn load_or_create(path: &Path) -> Result<Identity, IdentityError> {
    match std::fs::read(path) {
        Ok(bytes) => decode_identity(&bytes).ok_or_else(|| IdentityError::Corrupt(path.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => mint(path),
        Err(error) => Err(IdentityError::Io(path.to_owned(), error)),
    }
}

fn mint(path: &Path) -> Result<Identity, IdentityError> {
    let identity = leviculum_std::generate_identity();
    let encoded = encode_identity(&identity).ok_or(IdentityError::NotStorable)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| IdentityError::Io(parent.to_owned(), e))?;
    }
    std::fs::write(path, encoded).map_err(|e| IdentityError::Io(path.to_owned(), e))?;
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_first_run_mints_and_then_keeps_the_same_address() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/identity");

        let minted = load_or_create(&path).expect("first run mints");
        assert!(path.exists(), "the minted identity must be persisted");
        let reloaded = load_or_create(&path).expect("second run loads");
        assert_eq!(
            reloaded.hash(),
            minted.hash(),
            "a second run must keep the address the first one published"
        );
    }

    /// The inversion of `lnomad`'s behaviour, and the whole point of this
    /// module: a corrupt file must NOT quietly become a new address.
    #[test]
    fn a_corrupt_record_is_fatal_and_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("identity");
        std::fs::write(&path, b"not an identity record").expect("write");

        // `Identity` has no `Debug`, so the success arm cannot be unwrapped
        // into a panic message; match instead.
        let error = match load_or_create(&path) {
            Ok(_) => panic!("a corrupt record must not be replaced"),
            Err(error) => error,
        };
        assert!(matches!(error, IdentityError::Corrupt(_)), "{error}");
        assert_eq!(
            std::fs::read(&path).expect("read back"),
            b"not an identity record",
            "the file the user has to rescue must still be there"
        );
    }

    #[test]
    fn a_truncated_record_counts_as_corrupt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("identity");
        let full = encode_identity(&leviculum_std::generate_identity()).expect("encode");
        std::fs::write(&path, &full[..full.len() / 2]).expect("write");

        assert!(
            matches!(load_or_create(&path), Err(IdentityError::Corrupt(_))),
            "a half-written record is not an address to publish"
        );
    }

    #[test]
    fn the_home_env_var_wins_over_xdg() {
        // Serialised with the other env-reading test by running in one test
        // function: cargo runs test fns in parallel threads of one process,
        // and the environment is process-wide.
        let previous_home = std::env::var_os(HOME_ENV);
        std::env::set_var(HOME_ENV, "/tmp/lnmsg-home-test");
        assert_eq!(
            home_dir().expect("explicit home"),
            PathBuf::from("/tmp/lnmsg-home-test")
        );
        std::env::set_var(HOME_ENV, "");
        let xdg = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/lnmsg-xdg-test");
        assert_eq!(
            home_dir().expect("xdg home"),
            PathBuf::from("/tmp/lnmsg-xdg-test/lnmsg")
        );
        match xdg {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match previous_home {
            Some(value) => std::env::set_var(HOME_ENV, value),
            None => std::env::remove_var(HOME_ENV),
        }
    }
}
