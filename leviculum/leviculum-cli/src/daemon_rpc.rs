//! Reaching the daemon over the shared-instance RPC from a client tool.
//!
//! Both `lnstest diag` and `lnstest selftest` have to answer the same two
//! questions before they can ask a daemon anything: which shared instance to
//! talk to, and what authkey it expects. The answers are derived from the
//! config directory alone — the instance name from the config file, the
//! authkey as `SHA256(storage/transport_identity)` — so they live here rather
//! than in whichever command needed them first (Codeberg #190).
//!
//! The identity file is opened only to hash it. Its bytes never leave these
//! functions.

use std::path::{Path, PathBuf};

use leviculum_std::config::Config;

/// Load the config file under `config_dir`, if there is a readable one.
///
/// Returns `None` for both "no file" and "file present but unparseable"; a
/// caller that wants to distinguish them reads the file itself. Every value
/// derived from a config here has a defined answer without one.
pub fn load_config(config_dir: &Path) -> Option<Config> {
    let config_file = config_dir.join("config");
    if !config_file.exists() {
        return None;
    }
    Config::load(&config_file).ok()
}

/// The shared instance a client should address: an explicit override, else the
/// config's `instance_name`, else Python's `"default"`.
pub fn resolve_instance_name(over: Option<&str>, config: Option<&Config>) -> String {
    over.map(|s| s.to_string())
        .or_else(|| config.map(|c| c.reticulum.instance_name.clone()))
        .unwrap_or_else(|| "default".to_string())
}

/// Resolve the daemon's RPC authkey: `SHA256(storage/transport_identity)`.
///
/// Tries `{config_dir}/storage/transport_identity` first (the path `lnsd`
/// always uses unless `--storage` was given), then the config's
/// `storage_path` if set. The 64-byte file is hashed and discarded — its
/// bytes never leave this function.
pub fn resolve_authkey(
    config_dir: &Path,
    config: Option<&Config>,
) -> Result<([u8; 32], PathBuf), String> {
    let mut candidates: Vec<PathBuf> = vec![config_dir.join("storage").join("transport_identity")];
    if let Some(sp) = config.and_then(|c| c.reticulum.storage_path.as_ref()) {
        candidates.push(sp.join("transport_identity"));
    }
    let mut errors = Vec::new();
    for path in &candidates {
        match std::fs::read(path) {
            Ok(bytes) if bytes.len() == 64 => {
                use sha2::Digest;
                let digest = sha2::Sha256::digest(&bytes);
                let mut key = [0u8; 32];
                key.copy_from_slice(&digest);
                return Ok((key, path.clone()));
            }
            Ok(bytes) => errors.push(format!(
                "{}: unexpected size {} (expected 64)",
                path.display(),
                bytes.len()
            )),
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    Err(errors.join("; "))
}

/// Everything needed to issue a shared-instance query, resolved from a config
/// directory in one step.
pub struct DaemonAccess {
    /// Shared instance to address.
    pub instance_name: String,
    /// RPC authkey the daemon expects.
    pub authkey: [u8; 32],
}

impl DaemonAccess {
    /// Resolve access from a config directory, falling back to the platform
    /// default directory when none is given.
    ///
    /// Fails with a human-readable reason when the identity file cannot be
    /// read, which is the ordinary case for a tool pointed at a config
    /// directory that no daemon owns.
    pub fn resolve(config_dir: Option<&Path>, instance_name: Option<&str>) -> Result<Self, String> {
        let config_dir = config_dir
            .map(|p| p.to_path_buf())
            .unwrap_or_else(Config::default_config_dir);
        let config = load_config(&config_dir);
        let instance_name = resolve_instance_name(instance_name, config.as_ref());
        let (authkey, _) = resolve_authkey(&config_dir, config.as_ref())?;
        Ok(Self {
            instance_name,
            authkey,
        })
    }

    /// Run one shared-instance query.
    pub async fn query(&self, get: &str) -> Result<serde_json::Value, String> {
        leviculum_std::rpc_query(&self.instance_name, &self.authkey, get)
            .await
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_instance_name_wins_over_the_config_and_the_default() {
        assert_eq!(resolve_instance_name(Some("chosen"), None), "chosen");
        assert_eq!(resolve_instance_name(None, None), "default");
    }

    #[test]
    fn a_missing_identity_file_is_a_reason_not_a_panic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = DaemonAccess::resolve(Some(tmp.path()), Some("inst"))
            .err()
            .expect("no identity file was written");
        assert!(
            err.contains("transport_identity"),
            "the reason must name what it could not read: {err}"
        );
    }

    #[test]
    fn an_identity_file_of_the_wrong_size_is_rejected_by_size() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let storage = tmp.path().join("storage");
        std::fs::create_dir_all(&storage).expect("storage dir");
        std::fs::write(storage.join("transport_identity"), [0u8; 32]).expect("write identity");
        let err = DaemonAccess::resolve(Some(tmp.path()), Some("inst"))
            .err()
            .expect("wrong size");
        assert!(err.contains("unexpected size 32"), "{err}");
    }

    /// The authkey is `SHA256` of the identity file, and the identity bytes are
    /// not what is returned.
    #[test]
    fn the_authkey_is_the_hash_of_the_identity_file() {
        use sha2::Digest;
        let tmp = tempfile::tempdir().expect("tempdir");
        let storage = tmp.path().join("storage");
        std::fs::create_dir_all(&storage).expect("storage dir");
        let identity = [7u8; 64];
        std::fs::write(storage.join("transport_identity"), identity).expect("write identity");

        let access = DaemonAccess::resolve(Some(tmp.path()), Some("inst")).expect("resolves");
        let expected: [u8; 32] = sha2::Sha256::digest(identity).into();
        assert_eq!(access.authkey, expected);
        assert_eq!(access.instance_name, "inst");
    }
}
