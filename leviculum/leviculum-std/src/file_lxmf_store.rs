//! File-backed [`LxmfStorage`]: one file per key, in one directory.
//!
//! `leviculum-lxmf` is `no_std` and ships only in-memory and null storage, so
//! every host application that wants its LXMF queue to survive a restart
//! writes this same adapter. This is that adapter, written once.
//!
//! The layout follows the ratchet stores under
//! [`StdStorage`](crate::driver::StdStorage): a flat directory of
//! hex-named files, replaced through the same temp-file-and-rename write. Hex
//! is what makes a key a filename — an LXMF key is arbitrary bytes and the
//! router's own key (`lxmf/router-state`) already contains a separator, so a
//! key can never be used as a path component. Hex is injective, contains no
//! `/`, `.` or `..`, and does not depend on the filesystem's case folding.
//!
//! Writes are write-through: [`LxmfStorage::store`] has replaced the file by
//! the time it returns, which is what
//! [`LxmfRouter::persist`](leviculum_lxmf::router::LxmfRouter::persist) asks
//! for, so [`LxmfStorage::flush`] has nothing left to do.

use std::path::{Path, PathBuf};

use leviculum_lxmf::storage::{LxmfStorage, StorageError};

use crate::storage::{atomic_write, hex_decode, hex_encode};

/// A directory of LXMF key-value entries.
pub struct FileLxmfStorage {
    dir: PathBuf,
}

impl FileLxmfStorage {
    /// Open (creating it if needed) the directory holding the entries.
    ///
    /// `pub`, and deliberately the whole construction path: a downstream crate
    /// that persists an [`LxmfRouter`](leviculum_lxmf::router::LxmfRouter) has
    /// to be able to build one of these, and a `pub(crate)` constructor would
    /// leave the type visible and unusable.
    pub fn new<P: AsRef<Path>>(dir: P) -> Result<Self, StorageError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|_| StorageError::Io)?;
        Ok(Self { dir })
    }

    /// The directory the entries live in.
    pub fn path(&self) -> &Path {
        &self.dir
    }

    fn entry_path(&self, key: &[u8]) -> PathBuf {
        self.dir.join(hex_encode(key))
    }
}

impl LxmfStorage for FileLxmfStorage {
    fn load(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        match std::fs::read(self.entry_path(key)) {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(StorageError::Io),
        }
    }

    fn store(&mut self, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        // An empty key would name the directory itself. Nothing in the crate
        // produces one, and refusing it here keeps that true.
        if key.is_empty() {
            return Err(StorageError::Corrupt);
        }
        atomic_write(&self.entry_path(key), value).map_err(|_| StorageError::Io)
    }

    fn remove(&mut self, key: &[u8]) -> Result<(), StorageError> {
        match std::fs::remove_file(self.entry_path(key)) {
            Ok(()) => Ok(()),
            // Same answer as the in-memory store: removing what is not there
            // is a caller error, not a silent success.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(StorageError::NotFound)
            }
            Err(_) => Err(StorageError::Io),
        }
    }

    fn keys(&self, prefix: &[u8]) -> Result<Vec<Vec<u8>>, StorageError> {
        let directory = match std::fs::read_dir(&self.dir) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(StorageError::Io),
        };
        let mut keys = Vec::new();
        for entry in directory.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            // A rename that lost its race, or a file this store did not write.
            let Some(key) = hex_decode(&name) else {
                continue;
            };
            if key.starts_with(prefix) {
                keys.push(key);
            }
        }
        keys.sort();
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviculum_core::{Identity, MemoryStorage, NodeCoreBuilder};
    use leviculum_lxmf::{
        node::{LxmfNode, LxmfNodeConfig},
        router::{LxmfRouter, RouterConfig},
        DeliveryMethod,
    };
    use rand_core::OsRng;

    use crate::clock::SystemClock;

    fn identity_from(seed: u8) -> Identity {
        let mut private = [0u8; 64];
        for (index, byte) in private.iter_mut().enumerate() {
            *byte = seed.wrapping_add(index as u8);
        }
        Identity::from_private_key_bytes(&private).expect("deterministic identity")
    }

    #[test]
    fn entries_survive_dropping_and_reopening_the_store() {
        let dir = tempfile::tempdir().expect("temp dir");

        let mut storage = FileLxmfStorage::new(dir.path()).expect("open store");
        storage
            .store(b"lxmf/router-state", b"queue")
            .expect("store");
        storage.store(b"lxmf/peers/anna", b"Anna").expect("store");
        storage.store(b"lxmf/peers/bo", b"Bo").expect("store");
        storage.flush().expect("flush");
        drop(storage);

        let mut storage = FileLxmfStorage::new(dir.path()).expect("reopen store");
        assert_eq!(
            storage.load(b"lxmf/router-state").expect("load"),
            Some(b"queue".to_vec())
        );
        assert_eq!(
            storage.load(b"lxmf/peers/anna").expect("load"),
            Some(b"Anna".to_vec())
        );
        assert_eq!(
            storage.keys(b"lxmf/peers/").expect("keys"),
            vec![b"lxmf/peers/anna".to_vec(), b"lxmf/peers/bo".to_vec()]
        );
        assert_eq!(storage.keys(b"").expect("keys").len(), 3);

        // Overwriting is replacement, not append, and it too survives a reopen.
        storage
            .store(b"lxmf/router-state", b"newer queue")
            .expect("replace");
        storage.remove(b"lxmf/peers/bo").expect("remove");
        drop(storage);

        let storage = FileLxmfStorage::new(dir.path()).expect("reopen store");
        assert_eq!(
            storage.load(b"lxmf/router-state").expect("load"),
            Some(b"newer queue".to_vec())
        );
        assert_eq!(storage.load(b"lxmf/peers/bo").expect("load"), None);
        assert_eq!(
            storage.keys(b"lxmf/peers/").expect("keys"),
            vec![b"lxmf/peers/anna".to_vec()]
        );
    }

    #[test]
    fn absent_key_loads_as_none_and_refuses_removal() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut storage = FileLxmfStorage::new(dir.path()).expect("open store");

        assert_eq!(storage.load(b"absent").expect("load"), None);
        assert_eq!(storage.remove(b"absent"), Err(StorageError::NotFound));
        assert_eq!(storage.keys(b"").expect("keys"), Vec::<Vec<u8>>::new());
    }

    /// The reason the type exists: a queued message is still queued after a
    /// restart, with no shadow bookkeeping in the application.
    #[test]
    fn a_queued_message_survives_a_router_restart() {
        let dir = tempfile::tempdir().expect("temp dir");
        let identity = identity_from(1);
        let identity_hash = *identity.hash();
        let mut core =
            NodeCoreBuilder::new().build(OsRng, SystemClock::new(), MemoryStorage::with_defaults());
        let delivery = LxmfNode::delivery_destination(identity).expect("delivery destination");
        let node = LxmfNode::register(&mut core, delivery, LxmfNodeConfig::default())
            .expect("register delivery destination");
        let mut router = LxmfRouter::new(node, identity_hash, RouterConfig::default());
        let message = router
            .create_message(
                &core,
                [7; 16],
                b"title".to_vec(),
                b"content".to_vec(),
                Vec::new(),
                DeliveryMethod::Opportunistic,
            )
            .expect("compose message");
        let message_id = message.message_id;
        let _ = router.enqueue(&core, message).expect("queue message");

        let mut storage = FileLxmfStorage::new(dir.path()).expect("open store");
        router.persist(&mut storage).expect("persist queue");
        drop(router);
        drop(storage);

        let delivery = LxmfNode::delivery_destination(identity_from(1)).expect("delivery");
        let node = LxmfNode::register(&mut core, delivery, LxmfNodeConfig::default())
            .expect("re-register delivery destination");
        let mut restored = LxmfRouter::new(node, identity_hash, RouterConfig::default());
        let storage = FileLxmfStorage::new(dir.path()).expect("reopen store");
        restored.restore(&storage).expect("restore queue");

        assert_eq!(restored.outbound().len(), 1);
        assert_eq!(
            restored
                .outbound()
                .get(&message_id)
                .expect("restored entry")
                .message()
                .content,
            b"content".to_vec()
        );
    }
}
