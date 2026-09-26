//! Persistence boundary for LXMF state.
//!
//! `leviculum_core::Storage` is intentionally typed around Reticulum routing
//! state and has no application namespace. LXMF therefore uses this small
//! companion trait. Host implementations can map it to files/flash/database;
//! [`MemoryLxmfStorage`] is suitable for tests and transient embedded use.

use alloc::{collections::BTreeMap, vec::Vec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageError {
    Full,
    NotFound,
    Corrupt,
    Io,
}

impl core::fmt::Display for StorageError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Full => write!(f, "storage is full"),
            Self::NotFound => write!(f, "no such storage entry"),
            Self::Corrupt => write!(f, "stored data is corrupt"),
            Self::Io => write!(f, "storage I/O failed"),
        }
    }
}

impl core::error::Error for StorageError {}

pub trait LxmfStorage {
    fn load(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError>;
    fn store(&mut self, key: &[u8], value: &[u8]) -> Result<(), StorageError>;
    fn remove(&mut self, key: &[u8]) -> Result<(), StorageError>;
    fn keys(&self, prefix: &[u8]) -> Result<Vec<Vec<u8>>, StorageError>;
    fn flush(&mut self) -> Result<(), StorageError> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct MemoryLxmfStorage {
    entries: BTreeMap<Vec<u8>, Vec<u8>>,
    max_bytes: usize,
    bytes: usize,
}

impl MemoryLxmfStorage {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            max_bytes,
            bytes: 0,
        }
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Default for MemoryLxmfStorage {
    fn default() -> Self {
        Self::new(4 * 1024 * 1024)
    }
}

impl LxmfStorage for MemoryLxmfStorage {
    fn load(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(self.entries.get(key).cloned())
    }

    fn store(&mut self, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        // Account for both allocations retained by the map. Counting values
        // alone lets an attacker exhaust a constrained target with many large
        // keys carrying empty values while remaining below `max_bytes`.
        let previous = self
            .entries
            .get(key)
            .map_or(0, |previous| key.len().saturating_add(previous.len()));
        let replacement = key
            .len()
            .checked_add(value.len())
            .ok_or(StorageError::Full)?;
        let next = self
            .bytes
            .saturating_sub(previous)
            .checked_add(replacement)
            .ok_or(StorageError::Full)?;
        if next > self.max_bytes {
            return Err(StorageError::Full);
        }
        self.entries.insert(key.to_vec(), value.to_vec());
        self.bytes = next;
        Ok(())
    }

    fn remove(&mut self, key: &[u8]) -> Result<(), StorageError> {
        let removed = self.entries.remove(key).ok_or(StorageError::NotFound)?;
        self.bytes = self
            .bytes
            .saturating_sub(key.len().saturating_add(removed.len()));
        Ok(())
    }

    fn keys(&self, prefix: &[u8]) -> Result<Vec<Vec<u8>>, StorageError> {
        Ok(self
            .entries
            .keys()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoLxmfStorage;
impl LxmfStorage for NoLxmfStorage {
    fn load(&self, _key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(None)
    }
    fn store(&mut self, _key: &[u8], _value: &[u8]) -> Result<(), StorageError> {
        Ok(())
    }
    fn remove(&mut self, _key: &[u8]) -> Result<(), StorageError> {
        Ok(())
    }
    fn keys(&self, _prefix: &[u8]) -> Result<Vec<Vec<u8>>, StorageError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capacity_is_transactional() {
        let mut storage = MemoryLxmfStorage::new(5);
        storage.store(b"a", b"1234").unwrap();
        assert_eq!(storage.store(b"b", b"x"), Err(StorageError::Full));
        assert_eq!(storage.load(b"a").unwrap(), Some(b"1234".to_vec()));
        storage.store(b"a", b"1").unwrap();
        assert_eq!(storage.bytes(), 2);
    }

    #[test]
    fn capacity_counts_keys_as_well_as_values() {
        let mut storage = MemoryLxmfStorage::new(4);
        storage.store(b"long", b"").unwrap();
        assert_eq!(storage.bytes(), 4);
        assert_eq!(storage.store(b"x", b""), Err(StorageError::Full));
    }
}
