//! File-backed ratchet store (msgpack per file, Python-compatible).
//!
//! Known ratchets: one file per destination in `{storage}/ratchets/`
//! Dest ratchet keys: one file per destination in `{storage}/ratchetkeys/`
//! Filenames are hex-encoded truncated destination hashes.

use std::path::{Path, PathBuf};

use leviculum_core::constants::{RATCHET_SIZE, TRUNCATED_HASHBYTES};
use leviculum_core::ratchet_store::{KnownRatchetEntry, RatchetStore};

use crate::error::Error;
use crate::storage::{atomic_write, hex_decode, hex_encode};

pub(crate) const RATCHETS_DIR: &str = "ratchets";
pub(crate) const RATCHETKEYS_DIR: &str = "ratchetkeys";

fn encode_known_ratchet(ratchet_pub: &[u8; RATCHET_SIZE], received_secs: f64) -> Vec<u8> {
    let map = rmpv::Value::Map(vec![
        (
            rmpv::Value::String("ratchet".into()),
            rmpv::Value::Binary(ratchet_pub.to_vec()),
        ),
        (
            rmpv::Value::String("received".into()),
            rmpv::Value::F64(received_secs),
        ),
    ]);
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &map).expect("Vec write cannot fail");
    buf
}

fn decode_known_ratchet(data: &[u8]) -> Option<([u8; RATCHET_SIZE], f64)> {
    let value = rmpv::decode::read_value(&mut &data[..]).ok()?;
    let map = value.as_map()?;

    let mut ratchet: Option<[u8; RATCHET_SIZE]> = None;
    let mut received: Option<f64> = None;

    for (k, v) in map {
        let key_str = k.as_str()?;
        match key_str {
            "ratchet" => {
                let bytes = v.as_slice()?;
                if bytes.len() != RATCHET_SIZE {
                    return None;
                }
                let mut arr = [0u8; RATCHET_SIZE];
                arr.copy_from_slice(bytes);
                ratchet = Some(arr);
            }
            "received" => {
                received = Some(v.as_f64()?);
            }
            _ => {}
        }
    }

    Some((ratchet?, received?))
}

pub(crate) struct FileRatchetStore {
    ratchets_dir: PathBuf,
    ratchetkeys_dir: PathBuf,
}

impl FileRatchetStore {
    pub(crate) fn new(storage_dir: &Path) -> Self {
        Self {
            ratchets_dir: storage_dir.join(RATCHETS_DIR),
            ratchetkeys_dir: storage_dir.join(RATCHETKEYS_DIR),
        }
    }

    /// Delete a known ratchet file (for expiry). Not part of the trait.
    pub(crate) fn delete_known_ratchet(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) {
        let hex_name = hex_encode(dest_hash);
        let path = self.ratchets_dir.join(&hex_name);
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
    }

    fn ensure_dir(dir: &Path) {
        if !dir.exists() {
            let _ = std::fs::create_dir_all(dir);
        }
    }
}

impl RatchetStore for FileRatchetStore {
    type Error = Error;

    fn load_known_ratchets(
        &mut self,
    ) -> core::result::Result<Vec<([u8; TRUNCATED_HASHBYTES], KnownRatchetEntry)>, Error> {
        let dir = match std::fs::read_dir(&self.ratchets_dir) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::Storage(format!("ratchets dir: {e}"))),
        };

        let mut entries = Vec::new();
        for entry in dir.flatten() {
            let name = match entry.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };
            // Skip temp files
            if name.ends_with(".tmp") || name.ends_with(".out") {
                continue;
            }

            let hash_bytes = match hex_decode(&name) {
                Some(b) if b.len() == TRUNCATED_HASHBYTES => {
                    let mut arr = [0u8; TRUNCATED_HASHBYTES];
                    arr.copy_from_slice(&b);
                    arr
                }
                _ => continue,
            };

            let data = match std::fs::read(entry.path()) {
                Ok(d) => d,
                Err(_) => continue,
            };

            match decode_known_ratchet(&data) {
                Some((ratchet, received_secs)) => {
                    let now_secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs_f64())
                        .unwrap_or(0.0);
                    if now_secs - received_secs >= leviculum_core::constants::RATCHET_EXPIRY_SECS as f64 {
                        let _ = std::fs::remove_file(entry.path());
                        continue;
                    }
                    entries.push((
                        hash_bytes,
                        KnownRatchetEntry {
                            ratchet,
                            received_at_secs: received_secs,
                        },
                    ));
                }
                None => {
                    // Corrupted file, delete it
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        Ok(entries)
    }

    fn save_known_ratchet(
        &mut self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
        entry: &KnownRatchetEntry,
    ) -> core::result::Result<(), Error> {
        Self::ensure_dir(&self.ratchets_dir);
        let hex_name = hex_encode(dest_hash);
        let data = encode_known_ratchet(&entry.ratchet, entry.received_at_secs);
        atomic_write(&self.ratchets_dir.join(&hex_name), &data)
    }

    fn load_dest_ratchet_keys(
        &mut self,
    ) -> core::result::Result<Vec<([u8; TRUNCATED_HASHBYTES], Vec<u8>)>, Error> {
        let dir = match std::fs::read_dir(&self.ratchetkeys_dir) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::Storage(format!("ratchetkeys dir: {e}"))),
        };

        let mut entries = Vec::new();
        for entry in dir.flatten() {
            let name = match entry.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };
            if name.ends_with(".tmp") {
                continue;
            }

            let hash_bytes = match hex_decode(&name) {
                Some(b) if b.len() == TRUNCATED_HASHBYTES => {
                    let mut arr = [0u8; TRUNCATED_HASHBYTES];
                    arr.copy_from_slice(&b);
                    arr
                }
                _ => continue,
            };

            let data = match std::fs::read(entry.path()) {
                Ok(d) if !d.is_empty() => d,
                Ok(_) => {
                    let _ = std::fs::remove_file(entry.path());
                    continue;
                }
                Err(_) => continue,
            };

            entries.push((hash_bytes, data));
        }

        Ok(entries)
    }

    fn save_dest_ratchet_keys(
        &mut self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
        serialized: &[u8],
    ) -> core::result::Result<(), Error> {
        Self::ensure_dir(&self.ratchetkeys_dir);
        let hex_name = hex_encode(dest_hash);
        atomic_write(&self.ratchetkeys_dir.join(&hex_name), serialized)
    }
}
