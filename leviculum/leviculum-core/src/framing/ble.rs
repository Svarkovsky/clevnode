//! BLE fragmentation and defragmentation for Columba-compatible BLE interfaces.
//!
//! Implements the Columba Protocol v2.2 fragment format. Reticulum packets are
//! split into BLE-MTU-sized fragments with a 5-byte header, and reassembled on
//! the receiving side.
//!
//! # Fragment Format
//!
//! ```text
//! [Type:1][Sequence:2 BE][Total:2 BE][Payload...]
//! ```
//!
//! | Type | Value | Meaning | We send | We accept |
//! |------|-------|---------|---------|-----------|
//! | LONE | 0x00 | Complete packet in single fragment | no | yes |
//! | START | 0x01 | First fragment (also: the only fragment) | yes | yes |
//! | CONTINUE | 0x02 | Middle fragment | yes | yes |
//! | END | 0x03 | Last fragment | yes | yes |
//!
//! # Single-fragment packets
//!
//! A packet that fits in one fragment goes out as **START with `total = 1`**,
//! not as LONE. The reference implementation
//! (`ble-reticulum@07d94130`, `src/ble_reticulum/BLEFragmentation.py`) defines
//! exactly three types — `TYPE_START = 0x01`, `TYPE_CONTINUE = 0x02`,
//! `TYPE_END = 0x03` — and its reassembler raises
//! `ValueError: Invalid fragment type` on anything else, so a peer running it
//! discards every 0x00 fragment we emit. Since with a negotiated MTU of 517
//! essentially all ordinary traffic (a 167-byte announce included) is
//! single-fragment, emitting LONE silently drops nearly the whole outbound
//! direction.
//!
//! The reference's own `BLE_PROTOCOL_v2.2.md` disagrees with its code here: the
//! spec's sequence diagram renders a single fragment as `0x01+0x03`
//! ("START+END"). Its sender does no such thing — the type is assigned by an
//! `if i == 0 / elif i == num_fragments - 1 / else` chain, whose first branch
//! wins for `num_fragments == 1`, so a lone fragment leaves as a plain `0x01`.
//! **We follow the implementation, not the diagram**; a real peer runs the
//! code. Either reading rules out 0x00.
//!
//! LONE stays accepted on receive: older peers running our own firmware still
//! send it. Reassembly completes on fragment count rather than on seeing END,
//! so an inbound START with `total = 1` completes immediately.
//!
//! # Usage
//!
//! ```
//! use leviculum_core::framing::ble::*;
//!
//! // Fragment a packet for sending
//! let packet = b"Hello, Reticulum!";
//! let fragments = fragment_packet(packet, DEFAULT_MTU);
//! assert_eq!(fragments.len(), 1); // fits in a single START fragment
//!
//! // Defragment received data
//! let mut defrag = BleDefragmenter::new();
//! for frag in &fragments {
//!     if let DefragResult::Complete(data) = defrag.process(frag, 1000) {
//!         assert_eq!(&data, packet);
//!     }
//! }
//! ```

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

// Constants
/// Fragment header size in bytes.
pub const FRAGMENT_HEADER_SIZE: usize = 5;

/// Single-fragment packet (no fragmentation needed).
///
/// Accepted on receive for compatibility with older peers running our own
/// firmware, never emitted: the pinned reference rejects it. See the module
/// docs.
pub const FRAGMENT_TYPE_LONE: u8 = 0x00;
/// First fragment of a multi-fragment packet, and the type of a lone fragment.
pub const FRAGMENT_TYPE_START: u8 = 0x01;
/// Middle fragment of a multi-fragment packet.
pub const FRAGMENT_TYPE_CONTINUE: u8 = 0x02;
/// Last fragment of a multi-fragment packet.
pub const FRAGMENT_TYPE_END: u8 = 0x03;

/// BLE 4.0 minimum MTU.
pub const MIN_MTU: usize = 23;
/// Typical negotiated MTU for most devices.
pub const DEFAULT_MTU: usize = 185;
/// BLE 5.0 maximum MTU.
pub const MAX_MTU: usize = 517;
/// ATT protocol header overhead subtracted from MTU.
pub const ATT_HEADER_SIZE: usize = 3;

/// Timeout for incomplete fragment reassembly (milliseconds).
/// Sliding window: reset on each received fragment.
pub const REASSEMBLY_TIMEOUT_MS: u64 = 30_000;
/// Application-level keepalive interval (milliseconds).
pub const KEEPALIVE_INTERVAL_MS: u64 = 15_000;
/// Keepalive packet content (single zero byte).
pub const KEEPALIVE_BYTE: u8 = 0x00;

// Fragmentation (stateless)
/// Maximum payload bytes per fragment for a given BLE MTU.
///
/// Returns 0 if the MTU is too small to carry any payload.
pub const fn payload_per_fragment(mtu: usize) -> usize {
    let overhead = ATT_HEADER_SIZE + FRAGMENT_HEADER_SIZE;
    mtu.saturating_sub(overhead)
}

/// Number of fragments needed to send `data_len` bytes at the given BLE MTU.
///
/// Always returns at least 1 (a zero-length packet produces one fragment).
pub fn fragment_count(data_len: usize, mtu: usize) -> usize {
    let ppf = payload_per_fragment(mtu);
    if ppf == 0 {
        return 1; // degenerate MTU, send an empty single fragment
    }
    if data_len == 0 {
        return 1;
    }
    data_len.div_ceil(ppf)
}

/// Build the 5-byte header for fragment `index` of `total` fragments.
///
/// The type byte is determined by position, exactly as the reference sender
/// does it — so `total == 1` falls into the first branch and a lone fragment
/// is a START, never a LONE (see the module docs):
/// - index == 0: START (including the single-fragment case)
/// - index == total - 1: END
/// - otherwise: CONTINUE
pub fn build_fragment_header(index: usize, total: usize) -> [u8; FRAGMENT_HEADER_SIZE] {
    let ftype = if index == 0 {
        FRAGMENT_TYPE_START
    } else if index == total - 1 {
        FRAGMENT_TYPE_END
    } else {
        FRAGMENT_TYPE_CONTINUE
    };
    let seq = index as u16;
    let tot = total as u16;
    [
        ftype,
        (seq >> 8) as u8,
        seq as u8,
        (tot >> 8) as u8,
        tot as u8,
    ]
}

/// Get the payload slice for fragment `index` from `data`.
///
/// Returns the byte range of `data` that belongs to this fragment.
pub fn fragment_payload(data: &[u8], index: usize, mtu: usize) -> &[u8] {
    let ppf = payload_per_fragment(mtu);
    if ppf == 0 {
        return &[];
    }
    let start = index * ppf;
    let end = (start + ppf).min(data.len());
    if start >= data.len() {
        &[]
    } else {
        &data[start..end]
    }
}

/// Fragment a Reticulum packet into BLE fragments (convenience, uses alloc).
///
/// Each returned `Vec<u8>` is a complete fragment: 5-byte header + payload,
/// ready to write to the BLE TX characteristic.
pub fn fragment_packet(data: &[u8], mtu: usize) -> Vec<Vec<u8>> {
    let total = fragment_count(data.len(), mtu);
    let mut fragments = Vec::with_capacity(total);
    for i in 0..total {
        let header = build_fragment_header(i, total);
        let payload = fragment_payload(data, i, mtu);
        let mut frag = Vec::with_capacity(FRAGMENT_HEADER_SIZE + payload.len());
        frag.extend_from_slice(&header);
        frag.extend_from_slice(payload);
        fragments.push(frag);
    }
    fragments
}

// Defragmentation (stateful, per-peer)
/// Result of processing a received BLE fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefragResult {
    /// More fragments needed to complete the packet.
    NeedMore,
    /// All fragments received; contains the reassembled packet.
    Complete(Vec<u8>),
    /// Invalid fragment (bad type, too short, sequence out of range).
    Error,
}

/// Per-peer BLE fragment reassembler.
///
/// Accumulates fragments keyed by sequence number. Returns `Complete` when
/// all fragments (0..total-1) have been received. Supports out-of-order
/// delivery. The caller must provide the current time for timeout tracking.
///
/// # Cleanup
///
/// Call [`is_timed_out`](BleDefragmenter::is_timed_out) periodically and
/// [`reset`](BleDefragmenter::reset) when timed out, or rely on the automatic
/// reset when a new START/LONE fragment arrives while a previous reassembly
/// is in progress.
pub struct BleDefragmenter {
    fragments: BTreeMap<u16, Vec<u8>>,
    expected_total: u16,
    last_fragment_ms: u64,
    abandoned: u32,
    completed_fragments: u16,
}

impl BleDefragmenter {
    /// Create a new defragmenter with no pending reassembly.
    pub fn new() -> Self {
        Self {
            fragments: BTreeMap::new(),
            expected_total: 0,
            last_fragment_ms: 0,
            abandoned: 0,
            completed_fragments: 0,
        }
    }

    /// Process a received BLE fragment.
    ///
    /// `fragment` must be at least [`FRAGMENT_HEADER_SIZE`] bytes (5).
    /// `now_ms` is the current monotonic time in milliseconds.
    pub fn process(&mut self, fragment: &[u8], now_ms: u64) -> DefragResult {
        if fragment.len() < FRAGMENT_HEADER_SIZE {
            return DefragResult::Error;
        }

        let ftype = fragment[0];
        let seq = u16::from_be_bytes([fragment[1], fragment[2]]);
        let total = u16::from_be_bytes([fragment[3], fragment[4]]);
        let payload = &fragment[FRAGMENT_HEADER_SIZE..];

        // Validate fragment type
        if ftype > FRAGMENT_TYPE_END {
            return DefragResult::Error;
        }

        // Validate total > 0
        if total == 0 {
            return DefragResult::Error;
        }

        // Validate sequence < total
        if seq >= total {
            return DefragResult::Error;
        }

        // LONE fragment, complete packet in one piece
        if ftype == FRAGMENT_TYPE_LONE {
            self.discard_partial();
            self.expected_total = 0;
            self.completed_fragments = 1;
            return DefragResult::Complete(payload.to_vec());
        }

        // New multi-fragment sequence starting, reset any previous partial
        if ftype == FRAGMENT_TYPE_START && seq == 0 {
            self.discard_partial();
            self.expected_total = total;
        }

        // Check consistency with current reassembly
        if total != self.expected_total {
            // Fragment from a different packet or corrupted, discard
            self.discard_partial();
            self.expected_total = 0;
            return DefragResult::Error;
        }

        // Store fragment payload
        self.fragments.insert(seq, payload.to_vec());
        self.last_fragment_ms = now_ms;

        // Check if all fragments received
        if self.fragments.len() == self.expected_total as usize {
            // Reassemble in sequence order
            let mut packet = Vec::new();
            for i in 0..self.expected_total {
                match self.fragments.get(&i) {
                    Some(p) => packet.extend_from_slice(p),
                    None => {
                        // Should not happen, len check passed but gap found
                        self.reset();
                        return DefragResult::Error;
                    }
                }
            }
            self.completed_fragments = self.expected_total;
            self.reset();
            DefragResult::Complete(packet)
        } else {
            DefragResult::NeedMore
        }
    }

    /// How many fragments made up the most recently completed packet: 1
    /// for a LONE frame, the sequence's `total` otherwise; 0 before any
    /// completion. The RX log lines carry it (#376): whether a peer
    /// really fragments at 20 bytes although the negotiated ATT MTU
    /// allows far larger is visible nowhere else.
    pub fn last_completed_fragments(&self) -> u16 {
        self.completed_fragments
    }

    /// Drop an in-progress reassembly that a newer frame superseded, and
    /// count it: an abandoned head is a whole Reticulum packet lost, and
    /// before this counter existed the loss was invisible (#373 — two of
    /// five relayed two-fragment packets vanished with no drop line on
    /// any surface). Completion is not a discard and does not count.
    fn discard_partial(&mut self) {
        if !self.fragments.is_empty() {
            self.abandoned = self.abandoned.saturating_add(1);
            self.fragments.clear();
        }
    }

    /// How many in-progress reassemblies were discarded before they
    /// completed — a new START (or LONE) arrived over a partial head, or
    /// a fragment's `total` contradicted the reassembly in progress.
    /// Each count is one whole packet this receiver lost. Monotonic for
    /// the life of the defragmenter; callers log the delta (#373).
    pub fn abandoned_count(&self) -> u32 {
        self.abandoned
    }

    /// Fragments held by the reassembly in progress, 0 when idle. A
    /// caller that hard-resets on error uses this to tell "discarded a
    /// partial packet" from "nothing was pending" (#373).
    pub fn pending_fragments(&self) -> usize {
        self.fragments.len()
    }

    /// Discard the reassembly in progress and count it when one was
    /// pending: the caller is dropping a partial packet for a reason
    /// the defragmenter could not see (a hard reset after a garbage
    /// frame). Keeps [`abandoned_count`](BleDefragmenter::abandoned_count)
    /// the one total for "reassemblies this receiver lost" (#373).
    pub fn abandon(&mut self) {
        self.discard_partial();
        self.expected_total = 0;
        self.last_fragment_ms = 0;
    }

    /// Check if the current reassembly has timed out.
    ///
    /// Returns `true` if fragments are pending and the timeout has elapsed
    /// since the last fragment was received.
    pub fn is_timed_out(&self, now_ms: u64) -> bool {
        !self.fragments.is_empty()
            && now_ms.saturating_sub(self.last_fragment_ms) >= REASSEMBLY_TIMEOUT_MS
    }

    /// Discard any in-progress reassembly without counting it as an
    /// abandonment — the caller decided to drop it (timeout sweep,
    /// completion) and owns reporting it. In-stream discards the
    /// *defragmenter* decides count via
    /// [`abandoned_count`](BleDefragmenter::abandoned_count).
    pub fn reset(&mut self) {
        self.fragments.clear();
        self.expected_total = 0;
        self.last_fragment_ms = 0;
    }
}

impl Default for BleDefragmenter {
    fn default() -> Self {
        Self::new()
    }
}

// Tests
#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A packet that fits in one fragment must go out as START with total=1.
    ///
    /// Byte-level on purpose: the reference reassembler
    /// (`BLEFragmentation.py`, pinned commit 07d94130) knows only 0x01/0x02/
    /// 0x03 and raises `ValueError: Invalid fragment type` on 0x00, so the
    /// literal first byte is the wire contract. Asserted as `0x01`, not as
    /// `FRAGMENT_TYPE_START`, so redefining the constant cannot make this pass.
    #[test]
    fn test_single_fragment_is_start_on_the_wire() {
        let data = b"Hello, Reticulum!";
        let frags = fragment_packet(data, DEFAULT_MTU);
        assert_eq!(frags.len(), 1);

        // Header bytes: type=0x01 (START), seq=0x0000, total=0x0001
        assert_eq!(
            frags[0][..FRAGMENT_HEADER_SIZE],
            [0x01, 0x00, 0x00, 0x00, 0x01]
        );
        assert_ne!(
            frags[0][0], FRAGMENT_TYPE_LONE,
            "0x00 is rejected by the reference"
        );

        // Round-trip through our own defragmenter still completes.
        let mut defrag = BleDefragmenter::new();
        match defrag.process(&frags[0], 1000) {
            DefragResult::Complete(result) => assert_eq!(result, data),
            other => panic!("Expected Complete, got {:?}", other),
        }
    }

    /// A LONE fragment from an older peer still reassembles.
    ///
    /// Pins the receive-side compatibility the send-side change deliberately
    /// leaves in place: peers running our previous firmware still emit 0x00.
    #[test]
    fn test_received_lone_fragment_still_reassembles() {
        let data = b"Hello, Reticulum!";
        let mut lone = vec![FRAGMENT_TYPE_LONE, 0x00, 0x00, 0x00, 0x01];
        lone.extend_from_slice(data);

        let mut defrag = BleDefragmenter::new();
        match defrag.process(&lone, 1000) {
            DefragResult::Complete(result) => assert_eq!(result, data),
            other => panic!("Expected Complete, got {:?}", other),
        }
    }

    #[test]
    fn test_multi_fragment_roundtrip() {
        // 500 bytes at MTU 185 → payload_per_fragment = 177 → ceil(500/177) = 3 fragments
        let data: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
        let frags = fragment_packet(&data, DEFAULT_MTU);
        assert_eq!(frags.len(), 3);

        // Verify fragment types
        assert_eq!(frags[0][0], FRAGMENT_TYPE_START);
        assert_eq!(frags[1][0], FRAGMENT_TYPE_CONTINUE);
        assert_eq!(frags[2][0], FRAGMENT_TYPE_END);

        // Verify total count in each header
        for frag in &frags {
            assert_eq!(u16::from_be_bytes([frag[3], frag[4]]), 3);
        }

        // Reassemble in order
        let mut defrag = BleDefragmenter::new();
        assert_eq!(defrag.process(&frags[0], 1000), DefragResult::NeedMore);
        assert_eq!(defrag.process(&frags[1], 1000), DefragResult::NeedMore);
        match defrag.process(&frags[2], 1000) {
            DefragResult::Complete(result) => assert_eq!(result, data),
            other => panic!("Expected Complete, got {:?}", other),
        }
    }

    /// The RX lines' `frags=` source (#376): 1 for a LONE frame, the
    /// sequence total for a multi-fragment packet, 0 before anything
    /// completed, and the previous value survives NeedMore frames.
    #[test]
    fn last_completed_fragments_reports_how_the_peer_fragmented() {
        let mut defrag = BleDefragmenter::new();
        assert_eq!(defrag.last_completed_fragments(), 0, "nothing completed");

        let data: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
        let frags = fragment_packet(&data, DEFAULT_MTU);
        assert_eq!(frags.len(), 3);
        for frag in &frags[..2] {
            assert_eq!(defrag.process(frag, 1000), DefragResult::NeedMore);
            assert_eq!(defrag.last_completed_fragments(), 0);
        }
        assert!(matches!(
            defrag.process(&frags[2], 1000),
            DefragResult::Complete(_)
        ));
        assert_eq!(defrag.last_completed_fragments(), 3);

        // A LONE frame from an older peer counts as one fragment.
        let mut lone = vec![FRAGMENT_TYPE_LONE, 0x00, 0x00, 0x00, 0x01];
        lone.extend_from_slice(b"x");
        assert!(matches!(
            defrag.process(&lone, 1000),
            DefragResult::Complete(_)
        ));
        assert_eq!(defrag.last_completed_fragments(), 1);
    }

    #[test]
    fn test_out_of_order_reassembly() {
        let data: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
        let frags = fragment_packet(&data, DEFAULT_MTU);
        assert_eq!(frags.len(), 3);

        // Deliver out of order: START, END, CONTINUE
        let mut defrag = BleDefragmenter::new();
        assert_eq!(defrag.process(&frags[0], 1000), DefragResult::NeedMore);
        assert_eq!(defrag.process(&frags[2], 1000), DefragResult::NeedMore);
        match defrag.process(&frags[1], 1000) {
            DefragResult::Complete(result) => assert_eq!(result, data),
            other => panic!("Expected Complete, got {:?}", other),
        }
    }

    #[test]
    fn test_mtu_boundary_exact_fit() {
        // Payload that exactly fills one fragment: payload_per_fragment(185) = 177
        let ppf = payload_per_fragment(DEFAULT_MTU);
        let data: Vec<u8> = (0..ppf).map(|i| (i % 256) as u8).collect();
        let frags = fragment_packet(&data, DEFAULT_MTU);
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0][0], 0x01); // START, not LONE

        // One byte over → splits into 2 fragments
        let data2: Vec<u8> = (0..ppf + 1).map(|i| (i % 256) as u8).collect();
        let frags2 = fragment_packet(&data2, DEFAULT_MTU);
        assert_eq!(frags2.len(), 2);
        assert_eq!(frags2[0][0], FRAGMENT_TYPE_START);
        assert_eq!(frags2[1][0], FRAGMENT_TYPE_END);
    }

    #[test]
    fn test_large_packet() {
        let data: Vec<u8> = vec![0xAA; 500];
        let frags = fragment_packet(&data, DEFAULT_MTU);
        let ppf = payload_per_fragment(DEFAULT_MTU);

        // Verify payload sizes
        assert_eq!(frags[0].len() - FRAGMENT_HEADER_SIZE, ppf); // full
        assert_eq!(frags[1].len() - FRAGMENT_HEADER_SIZE, ppf); // full
        assert_eq!(frags[2].len() - FRAGMENT_HEADER_SIZE, 500 - 2 * ppf); // remainder

        // Reassemble and verify
        let mut defrag = BleDefragmenter::new();
        let mut result = DefragResult::NeedMore;
        for frag in &frags {
            result = defrag.process(frag, 1000);
        }
        match result {
            DefragResult::Complete(r) => assert_eq!(r, data),
            other => panic!("Expected Complete, got {:?}", other),
        }
    }

    #[test]
    fn test_timeout() {
        let data: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
        let frags = fragment_packet(&data, DEFAULT_MTU);

        let mut defrag = BleDefragmenter::new();
        assert_eq!(defrag.process(&frags[0], 1000), DefragResult::NeedMore);
        assert!(!defrag.is_timed_out(1000));
        assert!(!defrag.is_timed_out(30_999));
        assert!(defrag.is_timed_out(31_000));

        // After timeout, reset and try again
        defrag.reset();
        assert!(!defrag.is_timed_out(31_000)); // no pending fragments
    }

    #[test]
    fn test_zero_length_payload() {
        let data: &[u8] = &[];
        let frags = fragment_packet(data, DEFAULT_MTU);
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0].len(), FRAGMENT_HEADER_SIZE); // header only

        let mut defrag = BleDefragmenter::new();
        match defrag.process(&frags[0], 1000) {
            DefragResult::Complete(result) => assert!(result.is_empty()),
            other => panic!("Expected Complete, got {:?}", other),
        }
    }

    #[test]
    fn test_header_encoding() {
        let header = build_fragment_header(0x0102, 0x0304);
        // index > 0 and index < total-1 → CONTINUE
        assert_eq!(header[0], FRAGMENT_TYPE_CONTINUE);
        // Sequence: big-endian 0x0102
        assert_eq!(header[1], 0x01);
        assert_eq!(header[2], 0x02);
        // Total: big-endian 0x0304
        assert_eq!(header[3], 0x03);
        assert_eq!(header[4], 0x04);
    }

    #[test]
    fn test_too_short_fragment() {
        let mut defrag = BleDefragmenter::new();
        // Less than 5 bytes → Error
        assert_eq!(defrag.process(&[0x00, 0x01], 1000), DefragResult::Error);
        assert_eq!(defrag.process(&[], 1000), DefragResult::Error);
    }

    #[test]
    fn test_invalid_fragment_type() {
        let mut defrag = BleDefragmenter::new();
        let invalid = [0x04, 0x00, 0x00, 0x00, 0x01]; // type 4 doesn't exist
        assert_eq!(defrag.process(&invalid, 1000), DefragResult::Error);
    }

    #[test]
    fn test_sequence_exceeds_total() {
        let mut defrag = BleDefragmenter::new();
        // seq=1, total=1 → seq >= total → Error
        let bad = [FRAGMENT_TYPE_CONTINUE, 0x00, 0x01, 0x00, 0x01];
        assert_eq!(defrag.process(&bad, 1000), DefragResult::Error);
    }

    #[test]
    fn test_zero_total() {
        let mut defrag = BleDefragmenter::new();
        let bad = [FRAGMENT_TYPE_LONE, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(defrag.process(&bad, 1000), DefragResult::Error);
    }

    #[test]
    fn test_min_mtu() {
        // MIN_MTU 23 → payload = 23 - 3 - 5 = 15 bytes per fragment
        let data: Vec<u8> = (0..50).collect();
        let frags = fragment_packet(&data, MIN_MTU);
        let ppf = payload_per_fragment(MIN_MTU);
        assert_eq!(ppf, 15);
        assert_eq!(frags.len(), 4); // ceil(50/15) = 4

        let mut defrag = BleDefragmenter::new();
        let mut result = DefragResult::NeedMore;
        for frag in &frags {
            result = defrag.process(frag, 1000);
        }
        match result {
            DefragResult::Complete(r) => assert_eq!(r, data),
            other => panic!("Expected Complete, got {:?}", other),
        }
    }

    #[test]
    fn test_max_mtu() {
        // MAX_MTU 517 → payload = 517 - 3 - 5 = 509 bytes per fragment
        // A 500-byte packet fits in a single fragment, sent as START/total=1
        let data: Vec<u8> = vec![0xBB; 500];
        let frags = fragment_packet(&data, MAX_MTU);
        assert_eq!(frags.len(), 1);
        assert_eq!(
            frags[0][..FRAGMENT_HEADER_SIZE],
            [0x01, 0x00, 0x00, 0x00, 0x01]
        );

        let mut defrag = BleDefragmenter::new();
        match defrag.process(&frags[0], 1000) {
            DefragResult::Complete(r) => assert_eq!(r, data),
            other => panic!("Expected Complete, got {:?}", other),
        }
    }

    #[test]
    fn test_new_start_resets_previous() {
        let data1: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
        let data2: Vec<u8> = vec![0xFF; 500];
        let frags1 = fragment_packet(&data1, DEFAULT_MTU);
        let frags2 = fragment_packet(&data2, DEFAULT_MTU);

        let mut defrag = BleDefragmenter::new();
        // Start reassembling packet 1, then abandon it with packet 2's START
        assert_eq!(defrag.process(&frags1[0], 1000), DefragResult::NeedMore);
        assert_eq!(defrag.process(&frags2[0], 2000), DefragResult::NeedMore);
        assert_eq!(defrag.process(&frags2[1], 2000), DefragResult::NeedMore);
        match defrag.process(&frags2[2], 2000) {
            DefragResult::Complete(r) => assert_eq!(r, data2),
            other => panic!("Expected Complete, got {:?}", other),
        }
    }

    /// The #373 receiver-side property: when a peer's fragment streams
    /// interleave on one link — START(A), START(B), END(B) — the torn
    /// head A is dropped AND counted, and B still reassembles. Red
    /// until the counter existed: the drop happened silently, which is
    /// exactly how a relayed two-fragment packet could vanish with no
    /// line on any surface.
    #[test]
    fn test_interleaved_start_abandons_first_with_a_counter() {
        let a: Vec<u8> = (0..300).map(|i| (i % 256) as u8).collect();
        let b: Vec<u8> = vec![0xEE; 300];
        let frags_a = fragment_packet(&a, DEFAULT_MTU);
        let frags_b = fragment_packet(&b, DEFAULT_MTU);
        assert_eq!(frags_a.len(), 2);
        assert_eq!(frags_b.len(), 2);

        let mut defrag = BleDefragmenter::new();
        assert_eq!(defrag.process(&frags_a[0], 1000), DefragResult::NeedMore);
        assert_eq!(defrag.abandoned_count(), 0, "A is still in progress");
        assert_eq!(defrag.process(&frags_b[0], 1001), DefragResult::NeedMore);
        assert_eq!(defrag.abandoned_count(), 1, "A's head was discarded");
        match defrag.process(&frags_b[1], 1002) {
            DefragResult::Complete(r) => assert_eq!(r, b, "B reassembles"),
            other => panic!("Expected Complete, got {:?}", other),
        }
        assert_eq!(defrag.abandoned_count(), 1, "completion never counts");
    }

    /// Completing a packet is not an abandonment; neither is an idle
    /// START. The counter measures lost packets, nothing else.
    #[test]
    fn test_completion_and_idle_start_do_not_count_as_abandonment() {
        let data: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
        let mut defrag = BleDefragmenter::new();
        for _ in 0..3 {
            for frag in fragment_packet(&data, DEFAULT_MTU) {
                defrag.process(&frag, 1000);
            }
        }
        assert_eq!(defrag.abandoned_count(), 0);
    }

    /// A LONE (old-peer single fragment) over a partial head is also a
    /// discard of that head, and counts like one.
    #[test]
    fn test_lone_over_a_partial_head_counts_the_abandonment() {
        let a: Vec<u8> = vec![0xAA; 300];
        let frags_a = fragment_packet(&a, DEFAULT_MTU);
        let mut lone = vec![FRAGMENT_TYPE_LONE, 0x00, 0x00, 0x00, 0x01];
        lone.extend_from_slice(b"hello");

        let mut defrag = BleDefragmenter::new();
        assert_eq!(defrag.process(&frags_a[0], 1000), DefragResult::NeedMore);
        match defrag.process(&lone, 1001) {
            DefragResult::Complete(r) => assert_eq!(r, b"hello"),
            other => panic!("Expected Complete, got {:?}", other),
        }
        assert_eq!(defrag.abandoned_count(), 1);
    }

    /// A fragment whose `total` contradicts the reassembly in progress
    /// discards the partial (counted) and leaves the defragmenter clean
    /// for the next packet.
    #[test]
    fn test_total_mismatch_counts_and_resets_cleanly() {
        let a: Vec<u8> = vec![0xAA; 400];
        let frags_a = fragment_packet(&a, DEFAULT_MTU); // 3 fragments
        assert_eq!(frags_a.len(), 3);
        // A CONTINUE claiming total=2 amid a total=3 reassembly.
        let alien = [FRAGMENT_TYPE_CONTINUE, 0x00, 0x01, 0x00, 0x02];

        let mut defrag = BleDefragmenter::new();
        assert_eq!(defrag.process(&frags_a[0], 1000), DefragResult::NeedMore);
        assert_eq!(defrag.process(&alien, 1001), DefragResult::Error);
        assert_eq!(defrag.abandoned_count(), 1);
        assert_eq!(defrag.pending_fragments(), 0);

        // The next packet reassembles as if nothing happened.
        let b: Vec<u8> = vec![0xBB; 300];
        let mut result = DefragResult::NeedMore;
        for frag in fragment_packet(&b, DEFAULT_MTU) {
            result = defrag.process(&frag, 2000);
        }
        match result {
            DefragResult::Complete(r) => assert_eq!(r, b),
            other => panic!("Expected Complete, got {:?}", other),
        }
        assert_eq!(defrag.abandoned_count(), 1, "no further discards");
    }

    /// The limitation the counter cannot close, pinned so nobody
    /// mistakes it for a fixable receiver bug: the v2.2 header carries
    /// no packet id, so a torn head END-completed by the NEXT packet's
    /// tail with the same `total` is byte-for-byte indistinguishable
    /// from a legitimate reassembly and splices (#255's Columba-side
    /// glue, reproduced on our own receiver). The guarantee therefore
    /// lives on the transmitter: never interleave, and reset the peer
    /// via disconnect after a tear (`BLE_TX_RESYNC` in
    /// `leviculum-nrf/src/ble/notify.rs`).
    #[test]
    fn test_torn_head_plus_matching_tail_splices_undetectably() {
        let a: Vec<u8> = vec![0xAA; 300];
        let b: Vec<u8> = vec![0xBB; 300];
        let frags_a = fragment_packet(&a, DEFAULT_MTU);
        let frags_b = fragment_packet(&b, DEFAULT_MTU);

        let mut defrag = BleDefragmenter::new();
        assert_eq!(defrag.process(&frags_a[0], 1000), DefragResult::NeedMore);
        // B's START never arrives (sender tore mid-A and mid-B); B's END
        // carries seq=1 total=2, exactly what A's reassembly expects.
        match defrag.process(&frags_b[1], 1001) {
            DefragResult::Complete(spliced) => {
                assert_eq!(&spliced[..177], &a[..177], "A's head");
                assert_eq!(&spliced[177..], &b[177..], "B's tail");
            }
            other => panic!("Expected the splice, got {:?}", other),
        }
        assert_eq!(defrag.abandoned_count(), 0, "undetectable, uncounted");
    }

    #[test]
    fn test_payload_per_fragment_values() {
        assert_eq!(payload_per_fragment(MIN_MTU), 15);
        assert_eq!(payload_per_fragment(DEFAULT_MTU), 177);
        assert_eq!(payload_per_fragment(MAX_MTU), 509);
        assert_eq!(payload_per_fragment(8), 0); // MTU too small
        assert_eq!(payload_per_fragment(0), 0);
    }
}
