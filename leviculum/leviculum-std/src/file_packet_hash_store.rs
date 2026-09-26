//! File-backed packet hash store (msgpack, Python-compatible).

use std::path::{Path, PathBuf};

use leviculum_core::packet_hash_store::PacketHashStore;

use crate::error::Error;
use crate::packet_hashlist::{
    decode_packet_hashlist, encode_packet_hashlist, PACKET_HASHLIST_FILE,
};
use crate::storage::atomic_write;

pub(crate) struct FilePacketHashStore {
    path: PathBuf,
}

impl FilePacketHashStore {
    pub(crate) fn new(storage_dir: &Path) -> Self {
        Self {
            path: storage_dir.join(PACKET_HASHLIST_FILE),
        }
    }
}

impl PacketHashStore for FilePacketHashStore {
    type Error = Error;

    fn load_all(&mut self) -> core::result::Result<Vec<[u8; 32]>, Error> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let set = decode_packet_hashlist(&bytes)?;
                Ok(set.into_iter().collect())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(Error::Storage(format!("{e}"))),
        }
    }

    fn save_all(&mut self, hashes: &[[u8; 32]]) -> core::result::Result<(), Error> {
        let (encoded, _count) = encode_packet_hashlist(hashes.iter())?;
        atomic_write(&self.path, &encoded)
    }
}
