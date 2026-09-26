//! File-backed identity store for std targets.
//!
//! Stores the identity as raw 64 bytes at `{storage_path}/transport_identity`,
//! compatible with Python Reticulum (`rnsd`, `rnstatus`, etc.).

use leviculum_core::constants::IDENTITY_KEY_SIZE;
use leviculum_core::identity::Identity;
use leviculum_core::identity_store::IdentityStore;
use std::io::Write;
use std::path::{Path, PathBuf};

const IDENTITY_FILE: &str = "transport_identity";

/// File-backed identity store, Python-compatible.
///
/// The identity file is raw 64 bytes (32 X25519 + 32 Ed25519 private keys),
/// the same format Python Reticulum uses. No magic bytes, no checksum./// the filesystem provides existence checking.
pub struct FileIdentityStore {
    path: PathBuf,
}

impl FileIdentityStore {
    /// Create a store that reads/writes `{storage_dir}/transport_identity`.
    pub fn new(storage_dir: &Path) -> Self {
        Self {
            path: storage_dir.join(IDENTITY_FILE),
        }
    }
}

impl IdentityStore for FileIdentityStore {
    type Error = std::io::Error;

    /// Absence of the file is the only "no identity" answer.
    ///
    /// A file that exists but does not parse is a damaged key, not a missing
    /// one. Answering `Ok(None)` there makes the caller mint a fresh identity
    /// and save it over the same path, which destroys the evidence and the
    /// node's address in one step; a hard error keeps the damaged file for the
    /// operator to restore from backup.
    fn load(&mut self) -> Result<Option<Identity>, Self::Error> {
        match std::fs::read(&self.path) {
            Ok(bytes) if bytes.len() == IDENTITY_KEY_SIZE => {
                Identity::from_private_key_bytes(&bytes)
                    .map(Some)
                    .map_err(|e| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "{} holds {} bytes that are not a valid identity ({:?}); \
                             refusing to replace it, restore a backup or move the file aside",
                                self.path.display(),
                                IDENTITY_KEY_SIZE,
                                e
                            ),
                        )
                    })
            }
            Ok(bytes) => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{} has wrong size: {} (expected {}); \
                     refusing to replace it, restore a backup or move the file aside",
                    self.path.display(),
                    bytes.len(),
                    IDENTITY_KEY_SIZE
                ),
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Durable atomic write: the temp file's data reaches the disk before the
    /// rename, and the directory entry reaches the disk after it. Without both
    /// fsyncs a power cut can resurrect the file short or empty, which is
    /// exactly the state `load` now refuses to start on.
    fn save(&mut self, identity: &Identity) -> Result<(), Self::Error> {
        let bytes = identity
            .private_key_bytes()
            .map_err(|e| std::io::Error::other(format!("{:?}", e)))?;

        let tmp_path = self.path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp_path)?;
            f.write_all(&bytes)?;
            f.sync_data()?;
        }
        std::fs::rename(&tmp_path, &self.path)?;

        // Directory fsync: the rename itself is only durable once the parent
        // directory's entry is on disk.
        //
        // Unix only. Windows cannot open a directory as a file at all —
        // `File::open` on one fails with `ERROR_ACCESS_DENIED` (os error 5),
        // so this line did not merely skip the flush there, it failed the
        // save and took every builder test with it. There is no portable
        // equivalent to reach for: NTFS commits the rename's metadata as part
        // of the rename, and the durability this line buys on Unix is not
        // something a handle can ask for on Windows.
        #[cfg(unix)]
        if let Some(dir) = self.path.parent() {
            std::fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    }
}
