//! File-backed known destinations store (msgpack, Python-compatible).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use leviculum_core::constants::TRUNCATED_HASHBYTES;
use leviculum_core::known_destinations::{KnownDestEntry, KnownDestinationsStore};

use crate::error::Error;
use crate::known_destinations::{
    decode_known_destinations, encode_known_destinations, KNOWN_DESTINATIONS_FILE,
};
use crate::storage::atomic_write;

pub(crate) struct FileKnownDestinationsStore {
    path: PathBuf,
}

impl FileKnownDestinationsStore {
    pub(crate) fn new(storage_dir: &Path) -> Self {
        Self {
            path: storage_dir.join(KNOWN_DESTINATIONS_FILE),
        }
    }
}

impl KnownDestinationsStore for FileKnownDestinationsStore {
    type Error = Error;

    fn load_all(
        &mut self,
    ) -> core::result::Result<BTreeMap<[u8; TRUNCATED_HASHBYTES], KnownDestEntry>, Error> {
        match std::fs::read(&self.path) {
            Ok(bytes) => decode_known_destinations(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(Error::Storage(format!("{e}"))),
        }
    }

    fn save_all(
        &mut self,
        entries: &BTreeMap<[u8; TRUNCATED_HASHBYTES], KnownDestEntry>,
    ) -> core::result::Result<(), Error> {
        let encoded = encode_known_destinations(entries)?;
        atomic_write(&self.path, &encoded)
    }
}
