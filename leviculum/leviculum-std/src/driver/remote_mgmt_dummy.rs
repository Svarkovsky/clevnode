use std::sync::{Arc, Mutex};
use std::time::Instant;
use crate::driver::StdNodeCore;
use crate::driver::completions::CompletionRegistry;
use leviculum_core::link::LinkId;
use leviculum_core::constants::TRUNCATED_HASHBYTES;
use leviculum_core::TickOutput;
use crate::interfaces::inventory::SharedInventory;
use crate::interfaces::InterfaceStatsMap;

pub(crate) struct RemoteMgmtResponder {}

impl RemoteMgmtResponder {
    pub(crate) fn new(
        _iface_stats_map: InterfaceStatsMap,
        _inventory: SharedInventory,
        _start_time: Instant,
        _auto_peer_count: super::AutoPeerCount,
    ) -> Self {
        Self {}
    }

    pub(crate) fn handle_request(
        &self,
        _inner: &Arc<Mutex<StdNodeCore>>,
        _link_id: &LinkId,
        _request_id: &[u8; TRUNCATED_HASHBYTES],
        _path: &str,
        _data: &[u8],
        _completions: &CompletionRegistry,
    ) -> Option<TickOutput> {
        None
    }
}
