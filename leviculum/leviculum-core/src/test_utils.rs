//! Shared test utilities for leviculum-core.
//!
//! Provides deterministic clocks, mock interfaces, and transport helpers
//! to eliminate duplication across test modules.

use core::cell::Cell;

use alloc::vec::Vec;
use rand_core::OsRng;

use crate::identity::Identity;
use crate::memory_storage::MemoryStorage;
use crate::traits::{Clock, Interface, InterfaceError};
use crate::transport::{InterfaceId, Transport, TransportConfig};

/// Standard initial time for deterministic tests (1 second in ms).
pub(crate) const TEST_TIME_MS: u64 = 1_000_000;

/// Deterministic clock for tests, supports interior mutability via Cell.
pub(crate) struct MockClock(Cell<u64>);

impl MockClock {
    pub(crate) fn new(ms: u64) -> Self {
        Self(Cell::new(ms))
    }

    /// Advance time by the given number of milliseconds.
    pub(crate) fn advance(&self, ms: u64) {
        self.0.set(self.0.get() + ms);
    }

    /// Set time to an absolute value.
    pub(crate) fn set(&self, ms: u64) {
        self.0.set(ms);
    }
}

impl Clock for MockClock {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }
}

/// Mock interface for testing, records sent packets.
pub(crate) struct MockInterface {
    name: &'static str,
    id: InterfaceId,
    pub(crate) sent: Vec<Vec<u8>>,
    pub(crate) online: bool,
    /// When true, try_send() returns BufferFull instead of accepting.
    pub(crate) reject_sends: bool,
}

impl MockInterface {
    pub(crate) fn new(name: &'static str, id: u8) -> Self {
        Self {
            name,
            id: InterfaceId(id as usize),
            sent: Vec::new(),
            online: true,
            reject_sends: false,
        }
    }
}

impl Interface for MockInterface {
    fn id(&self) -> InterfaceId {
        self.id
    }
    fn name(&self) -> &str {
        self.name
    }
    fn mtu(&self) -> usize {
        500
    }
    fn is_online(&self) -> bool {
        self.online
    }
    fn try_send(&mut self, data: &[u8]) -> Result<(), InterfaceError> {
        if self.reject_sends {
            return Err(InterfaceError::BufferFull);
        }
        self.sent.push(data.to_vec());
        Ok(())
    }
}

/// Bare Transport with MockClock at TEST_TIME_MS, MemoryStorage, no interfaces.
pub(crate) fn test_transport() -> Transport<MockClock, MemoryStorage> {
    let clock = MockClock::new(TEST_TIME_MS);
    let identity = Identity::generate(&mut OsRng);
    Transport::new(
        TransportConfig::default(),
        clock,
        MemoryStorage::with_defaults(),
        identity,
    )
}
