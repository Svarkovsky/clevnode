//! Python-compatible known_destinations persistence
//!
//! Format: msgpack map {dest_hash(16 bytes) → [timestamp(f64), packet_hash(32 bytes),
//! public_key(64 bytes), app_data(bytes or nil)]}
//!
//! Python ref: Identity.py:93-103 (remember), Identity.py:163-216 (save),
//! Identity.py:220-236 (load)

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use leviculum_core::constants::{IDENTITY_KEY_SIZE, TRUNCATED_HASHBYTES};
pub(crate) use leviculum_core::known_destinations::{KnownDestEntry, PACKET_HASH_LEN};

pub(crate) const KNOWN_DESTINATIONS_FILE: &str = "known_destinations";

/// Decode a known_destinations msgpack blob into entries.
///
/// Python format: msgpack map where keys are 16-byte binary (dest hashes)
/// and values are arrays: [f64, bin32, bin64, bin_or_nil].
pub(crate) fn decode_known_destinations(
    data: &[u8],
) -> Result<BTreeMap<[u8; TRUNCATED_HASHBYTES], KnownDestEntry>> {
    let value: rmpv::Value = rmpv::decode::read_value(&mut &data[..])
        .map_err(|e| Error::Serialization(format!("msgpack decode error: {e}")))?;

    let map = value
        .as_map()
        .ok_or_else(|| Error::Serialization("known_destinations: expected map".into()))?;

    let mut entries = BTreeMap::new();
    for (key, val) in map {
        // Key: 16-byte binary
        let key_bytes = match key.as_slice() {
            Some(b) if b.len() == TRUNCATED_HASHBYTES => {
                let mut arr = [0u8; TRUNCATED_HASHBYTES];
                arr.copy_from_slice(b);
                arr
            }
            _ => continue, // Skip invalid keys (Python does the same length check)
        };

        // Value: array of at least 4 elements. rnsd writes 5-element values
        // `[ts, packet_hash, pubkey, app_data, 0]` (Identity.py:107); accept
        // those, reading the first four fields and ignoring trailing elements.
        let arr = match val.as_array() {
            Some(a) if a.len() >= 4 => a,
            _ => continue,
        };

        // [0] timestamp: f64
        let timestamp = match &arr[0] {
            rmpv::Value::F64(f) => *f,
            rmpv::Value::F32(f) => *f as f64,
            rmpv::Value::Integer(i) => i.as_f64().unwrap_or(0.0),
            _ => continue,
        };

        // [1] packet_hash: binary (typically 32 bytes)
        let packet_hash = match arr[1].as_slice() {
            Some(b) => b.to_vec(),
            None => vec![0u8; PACKET_HASH_LEN],
        };

        // [2] public_key: binary (64 bytes)
        let public_key = match arr[2].as_slice() {
            Some(b) if b.len() == IDENTITY_KEY_SIZE => {
                let mut arr = [0u8; IDENTITY_KEY_SIZE];
                arr.copy_from_slice(b);
                arr
            }
            _ => continue,
        };

        // [3] app_data: binary or nil
        let app_data = match &arr[3] {
            rmpv::Value::Nil => None,
            rmpv::Value::Binary(b) => {
                if b.is_empty() {
                    None
                } else {
                    Some(b.clone())
                }
            }
            _ => None,
        };

        entries.insert(
            key_bytes,
            KnownDestEntry {
                timestamp,
                packet_hash,
                public_key,
                app_data,
            },
        );
    }

    Ok(entries)
}

/// Encode entries into Python-compatible msgpack format.
pub(crate) fn encode_known_destinations(
    entries: &BTreeMap<[u8; TRUNCATED_HASHBYTES], KnownDestEntry>,
) -> Result<Vec<u8>> {
    use rmpv::Value;

    // Build a msgpack map: Vec<(Value, Value)>
    let pairs: Vec<(Value, Value)> = entries
        .iter()
        .map(|(hash, entry)| {
            let key = Value::Binary(hash.to_vec());
            let val = Value::Array(vec![
                Value::F64(entry.timestamp),
                Value::Binary(entry.packet_hash.clone()),
                Value::Binary(entry.public_key.to_vec()),
                match &entry.app_data {
                    Some(data) => Value::Binary(data.clone()),
                    None => Value::Nil,
                },
            ]);
            (key, val)
        })
        .collect();

    let map = Value::Map(pairs);
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &map)
        .map_err(|e| Error::Serialization(format!("msgpack encode error: {e}")))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::{SystemTime, UNIX_EPOCH};

    use leviculum_core::DestinationHash;
    use leviculum_core::Identity;

    use crate::storage::Storage;

    /// Test-only helper matching the old KnownDestinationsStore.
    struct KnownDestinationsStore {
        entries: BTreeMap<[u8; TRUNCATED_HASHBYTES], KnownDestEntry>,
    }

    impl KnownDestinationsStore {
        fn load(storage: &Storage) -> Self {
            let entries = match storage.read_root(KNOWN_DESTINATIONS_FILE) {
                Ok(bytes) => decode_known_destinations(&bytes).unwrap_or_default(),
                Err(_) => BTreeMap::new(),
            };
            Self { entries }
        }

        fn save(&self, storage: &Storage) -> Result<()> {
            let mut merged = self.entries.clone();
            if let Ok(bytes) = storage.read_root(KNOWN_DESTINATIONS_FILE) {
                if let Ok(disk_entries) = decode_known_destinations(&bytes) {
                    for (hash, entry) in disk_entries {
                        merged.entry(hash).or_insert(entry);
                    }
                }
            }
            let encoded = encode_known_destinations(&merged)?;
            storage.write_root(KNOWN_DESTINATIONS_FILE, &encoded)?;
            Ok(())
        }

        fn identities(&self) -> impl Iterator<Item = (DestinationHash, Identity)> + '_ {
            self.entries.iter().filter_map(|(hash, entry)| {
                let identity = Identity::from_public_key_bytes(&entry.public_key).ok()?;
                Some((DestinationHash::new(*hash), identity))
            })
        }

        fn len(&self) -> usize {
            self.entries.len()
        }

        fn update_from_announce(
            &mut self,
            dest_hash: &[u8; TRUNCATED_HASHBYTES],
            public_key: &[u8; IDENTITY_KEY_SIZE],
            app_data: &[u8],
        ) {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            self.entries.insert(
                *dest_hash,
                KnownDestEntry {
                    timestamp,
                    packet_hash: vec![0u8; PACKET_HASH_LEN],
                    public_key: *public_key,
                    app_data: if app_data.is_empty() {
                        None
                    } else {
                        Some(app_data.to_vec())
                    },
                },
            );
        }

        fn merge_from_storage<'a>(
            &mut self,
            iter: impl Iterator<Item = (&'a [u8; TRUNCATED_HASHBYTES], &'a Identity)>,
        ) {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            for (dest_hash, identity) in iter {
                self.entries
                    .entry(*dest_hash)
                    .or_insert_with(|| KnownDestEntry {
                        timestamp,
                        packet_hash: vec![0u8; PACKET_HASH_LEN],
                        public_key: identity.public_key_bytes(),
                        app_data: None,
                    });
            }
        }
    }

    #[test]
    fn test_empty_store() {
        let store = KnownDestinationsStore {
            entries: BTreeMap::new(),
        };
        assert_eq!(store.len(), 0);
        assert_eq!(store.identities().count(), 0);
    }

    #[test]
    fn test_encode_decode_round_trip() {
        let mut entries = BTreeMap::new();
        let hash = [0xAA; TRUNCATED_HASHBYTES];
        let identity = Identity::generate(&mut rand_core::OsRng);
        entries.insert(
            hash,
            KnownDestEntry {
                timestamp: 1708300000.0,
                packet_hash: vec![0xBB; PACKET_HASH_LEN],
                public_key: identity.public_key_bytes(),
                app_data: Some(b"test_app".to_vec()),
            },
        );

        let encoded = encode_known_destinations(&entries).unwrap();
        let decoded = decode_known_destinations(&encoded).unwrap();

        assert_eq!(decoded.len(), 1);
        let entry = &decoded[&hash];
        assert!((entry.timestamp - 1708300000.0).abs() < f64::EPSILON);
        assert_eq!(entry.packet_hash, vec![0xBB; PACKET_HASH_LEN]);
        assert_eq!(entry.public_key, identity.public_key_bytes());
        assert_eq!(entry.app_data, Some(b"test_app".to_vec()));
    }

    #[test]
    fn test_encode_decode_nil_app_data() {
        let mut entries = BTreeMap::new();
        let hash = [0xCC; TRUNCATED_HASHBYTES];
        let identity = Identity::generate(&mut rand_core::OsRng);
        entries.insert(
            hash,
            KnownDestEntry {
                timestamp: 1708300000.0,
                packet_hash: vec![0; PACKET_HASH_LEN],
                public_key: identity.public_key_bytes(),
                app_data: None,
            },
        );

        let encoded = encode_known_destinations(&entries).unwrap();
        let decoded = decode_known_destinations(&encoded).unwrap();
        assert_eq!(decoded[&hash].app_data, None);
    }

    #[test]
    fn test_extract_identities() {
        let mut entries = BTreeMap::new();
        let id1 = Identity::generate(&mut rand_core::OsRng);
        let id2 = Identity::generate(&mut rand_core::OsRng);
        let hash1 = [0x01; TRUNCATED_HASHBYTES];
        let hash2 = [0x02; TRUNCATED_HASHBYTES];

        entries.insert(
            hash1,
            KnownDestEntry {
                timestamp: 0.0,
                packet_hash: vec![0; PACKET_HASH_LEN],
                public_key: id1.public_key_bytes(),
                app_data: None,
            },
        );
        entries.insert(
            hash2,
            KnownDestEntry {
                timestamp: 0.0,
                packet_hash: vec![0; PACKET_HASH_LEN],
                public_key: id2.public_key_bytes(),
                app_data: None,
            },
        );

        let store = KnownDestinationsStore { entries };
        let identities: Vec<_> = store.identities().collect();
        assert_eq!(identities.len(), 2);
    }

    #[test]
    fn test_update_from_announce() {
        let mut store = KnownDestinationsStore {
            entries: BTreeMap::new(),
        };
        let id = Identity::generate(&mut rand_core::OsRng);
        let hash = [0xDD; TRUNCATED_HASHBYTES];
        let pubkey = id.public_key_bytes();

        store.update_from_announce(&hash, &pubkey, b"my_app");
        assert_eq!(store.len(), 1);
        assert!(store.entries[&hash].timestamp > 0.0);
        assert_eq!(store.entries[&hash].app_data, Some(b"my_app".to_vec()));
    }

    #[test]
    fn test_merge_from_storage() {
        let mut store = KnownDestinationsStore {
            entries: BTreeMap::new(),
        };

        // Pre-existing entry
        let existing_id = Identity::generate(&mut rand_core::OsRng);
        let existing_hash = [0x01; TRUNCATED_HASHBYTES];
        store.update_from_announce(&existing_hash, &existing_id.public_key_bytes(), b"");

        // New identity from storage
        let new_id = Identity::generate(&mut rand_core::OsRng);
        let new_hash = [0x02; TRUNCATED_HASHBYTES];
        let storage_identities: Vec<([u8; TRUNCATED_HASHBYTES], Identity)> =
            vec![(new_hash, new_id)];
        let refs: Vec<(&[u8; TRUNCATED_HASHBYTES], &Identity)> =
            storage_identities.iter().map(|(h, i)| (h, i)).collect();

        store.merge_from_storage(refs.into_iter());
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn test_merge_from_storage_no_overwrite() {
        let mut store = KnownDestinationsStore {
            entries: BTreeMap::new(),
        };

        let id = Identity::generate(&mut rand_core::OsRng);
        let hash = [0x01; TRUNCATED_HASHBYTES];
        store.update_from_announce(&hash, &id.public_key_bytes(), b"original_app");

        // Merge with the same hash from storage, should NOT overwrite
        let storage_entries: Vec<([u8; TRUNCATED_HASHBYTES], Identity)> = vec![(hash, id)];
        let refs: Vec<(&[u8; TRUNCATED_HASHBYTES], &Identity)> =
            storage_entries.iter().map(|(h, i)| (h, i)).collect();

        store.merge_from_storage(refs.into_iter());
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.entries[&hash].app_data,
            Some(b"original_app".to_vec()),
            "merge should not overwrite existing entries"
        );
    }

    /// rnsd writes 5-element values `[ts, packet_hash, pubkey, app_data, 0]`
    /// (Identity.py:107). The decoder must accept them, reading the first four
    /// fields and ignoring the trailing element, so a warm rnsd identity cache
    /// survives a swap to lnsd.
    #[test]
    fn test_decode_rnsd_five_element_entry() {
        use rmpv::Value;

        let hash = [0x11; TRUNCATED_HASHBYTES];
        let identity = Identity::generate(&mut rand_core::OsRng);
        let pubkey = identity.public_key_bytes();

        let val = Value::Array(vec![
            Value::from(1708300000u64),
            Value::Binary(vec![0x22; PACKET_HASH_LEN]),
            Value::Binary(pubkey.to_vec()),
            Value::Binary(b"app".to_vec()),
            Value::from(0u64),
        ]);
        let map = Value::Map(vec![(Value::Binary(hash.to_vec()), val)]);
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, &map).unwrap();

        let decoded = decode_known_destinations(&buf).unwrap();
        assert_eq!(decoded.len(), 1, "5-element rnsd entry must be accepted");
        let entry = &decoded[&hash];
        assert_eq!(entry.public_key, pubkey);
        assert_eq!(entry.packet_hash, vec![0x22; PACKET_HASH_LEN]);
        assert_eq!(entry.app_data, Some(b"app".to_vec()));
    }

    /// A canonical 4-element entry (our own encoding) must still decode.
    #[test]
    fn test_decode_four_element_entry_still_works() {
        use rmpv::Value;

        let hash = [0x33; TRUNCATED_HASHBYTES];
        let identity = Identity::generate(&mut rand_core::OsRng);
        let pubkey = identity.public_key_bytes();

        let val = Value::Array(vec![
            Value::F64(1708300000.0),
            Value::Binary(vec![0x44; PACKET_HASH_LEN]),
            Value::Binary(pubkey.to_vec()),
            Value::Nil,
        ]);
        let map = Value::Map(vec![(Value::Binary(hash.to_vec()), val)]);
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, &map).unwrap();

        let decoded = decode_known_destinations(&buf).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[&hash].public_key, pubkey);
        assert_eq!(decoded[&hash].app_data, None);
    }

    #[test]
    fn test_python_known_destinations_compat() {
        // Read the actual Python rnsd known_destinations file if present
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let Some(home) = home else { return };
        let path = home.join(".reticulum/storage/known_destinations");
        if !path.exists() {
            return;
        }

        let bytes = std::fs::read(&path).unwrap();
        let entries = decode_known_destinations(&bytes).unwrap();
        assert!(
            !entries.is_empty(),
            "Python known_destinations should have entries"
        );

        // Verify all entries have valid structure
        for (hash, entry) in &entries {
            assert_eq!(hash.len(), TRUNCATED_HASHBYTES);
            assert_eq!(entry.public_key.len(), IDENTITY_KEY_SIZE);
            assert!(entry.timestamp > 0.0);
        }

        // Verify we can extract valid identities
        let store = KnownDestinationsStore { entries };
        let identities: Vec<_> = store.identities().collect();
        assert!(
            !identities.is_empty(),
            "should extract at least one valid identity from Python file"
        );

        // Re-encode and decode to verify round-trip
        let re_encoded = encode_known_destinations(&store.entries).unwrap();
        let re_decoded = decode_known_destinations(&re_encoded).unwrap();
        assert_eq!(
            store.entries.len(),
            re_decoded.len(),
            "round-trip should preserve entry count"
        );
    }

    #[test]
    fn test_save_and_load_via_storage() {
        let path = std::env::temp_dir().join(format!("reticulum_test_kd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let storage = Storage::new(&path).unwrap();

        let mut store = KnownDestinationsStore {
            entries: BTreeMap::new(),
        };
        let id = Identity::generate(&mut rand_core::OsRng);
        let hash = [0xEE; TRUNCATED_HASHBYTES];
        store.update_from_announce(&hash, &id.public_key_bytes(), b"test");
        store.save(&storage).unwrap();

        let loaded = KnownDestinationsStore::load(&storage);
        assert_eq!(loaded.len(), 1);
        let identities: Vec<_> = loaded.identities().collect();
        assert_eq!(identities.len(), 1);
        assert_eq!(identities[0].0.into_bytes(), hash);

        let _ = std::fs::remove_dir_all(&path);
    }
}
