//! The pure half of the Columba BLE interface: link admission, per-link
//! framing state, keepalive and expiry policy, and the outbound fan-out
//! plan. No BlueZ, no tokio, no clock — the caller passes `now_ms` and
//! performs what the returned values ask for, which is what makes every
//! state here reachable from a host test (the same split as
//! `leviculum-ble-tx` and for the same reason).
//!
//! Wire rules are not restated here: fragmentation and reassembly come
//! from `leviculum_core::framing::ble`, the advertisement parse and the
//! connection-direction rule from `leviculum_ble_tx::peer`. This module
//! adds only what a dual-role host with more than one live link needs on
//! top: who is admitted, who is expired, and which bytes go to which link.
//!
//! # One interface, one broadcast domain
//!
//! All live BLE links belong to one Reticulum interface. An outbound
//! packet fans out to every link (the firmware's fan-out answers the same
//! question — see the #255 seam report); inbound packets from every link
//! feed the same interface. This also matches what BlueZ can actually do
//! on the peripheral side: a GATT notification goes to every subscribed
//! central at once, so the peripheral TX pipe *is* a broadcast domain and
//! [`TxPlan::notify_fragments`] is computed once at the minimum MTU across
//! peripheral links rather than per link.

use leviculum_ble_tx::{
    addr_value, effective_tx_gap_ms, judge_duplicate, parse_peer_advertisement, should_initiate,
    CandidateTable, ConnectDecision, Duplicate, Origin, ScanMode, TxGap, MANUFACTURER_DATA_LEN,
    SCAN_FALLBACK_AFTER_MS, SCAN_WINDOW_COLLECT_MS, WINDOW_CANDIDATES,
};
use leviculum_core::framing::ble::{
    fragment_packet, BleDefragmenter, DefragResult, FRAGMENT_HEADER_SIZE, KEEPALIVE_INTERVAL_MS,
    MIN_MTU,
};

/// A BLE address in display order (`AA:BB:CC:DD:EE:FF` → `[0xAA, …]`),
/// the byte order `bluer::Address` carries.
pub(crate) type Addr = [u8; 6];

/// The identity hash a Columba peer publishes and handshakes with.
pub(crate) type IdentityHash = [u8; 16];

/// Our capability flags: dual-role, full capability (v0.3.0 flags 0x00).
/// Advertised in the manufacturer record *and* fed to the connection
/// decision from one constant, so the two cannot disagree (the firmware
/// pins its `LOCAL_CAPS` the same way).
pub(crate) const LOCAL_CAPS: u8 = 0x00;

/// Default cap on simultaneous BLE links, both roles counted together.
/// A policy bound, not a resource one: BlueZ has no SoftDevice-style hard
/// connection slots, but every link costs airtime and the protocol's
/// practical ceiling is 3-4 reliable links (see
/// `docs/src/concepts/bluetooth-interfaces.md`). Matches the firmware's
/// `MAX_LINKS`.
pub(crate) const DEFAULT_MAX_LINKS: usize = 4;

/// A link whose peer has been silent this long is torn down. Three missed
/// keepalives at the protocol's 15 s cadence: one lost keepalive must not
/// cost a link, and BlueZ surfaces no supervision-timeout event for
/// peripheral-role links, so this timer is the only down-detector that
/// covers both roles.
///
/// Taken from the firmware's registry since #382, which made the same
/// number the bound on when a duplicate may displace a link: a link the
/// duplicate rule calls dead is exactly a link [`LinkTable::expire`] is
/// about to remove anyway, and two copies of that number in one
/// repository would not stay equal.
pub(crate) use leviculum_ble_tx::LINK_TIMEOUT_MS;

/// A central that connects but never writes its 16-byte identity is
/// disconnected after this long — the reference's
/// `_pending_identity_timeout` (`ble-reticulum@07d94130` `BLEInterface.py`, `_pending_identity_timeout`).
pub(crate) const HANDSHAKE_TIMEOUT_MS: u64 = 30_000;

/// Our side of a link: `Central` when we initiated the connection,
/// `Peripheral` when the peer connected to our GATT server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Central,
    Peripheral,
}

impl Role {
    /// Stable token for the structured log lines.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Role::Central => "central",
            Role::Peripheral => "peripheral",
        }
    }

    /// Who opened this connection, the input the duplicate rule turns
    /// on (#382). Our role IS the direction: we are the peripheral only
    /// when the peer dialled us, the central only when we dialled it.
    pub(crate) fn origin(self) -> Origin {
        match self {
            Role::Central => Origin::Outgoing,
            Role::Peripheral => Origin::Incoming,
        }
    }

    /// Stable token for `BLE_LINK_DUP origin=`, the firmware's spelling.
    pub(crate) fn origin_as_str(self) -> &'static str {
        match self.origin() {
            Origin::Incoming => "incoming",
            Origin::Outgoing => "outgoing",
        }
    }
}

/// A link torn down because a newer connection of the same identity
/// took it over (#376), and the silence that justified it (#382) — what
/// the driver needs to disconnect the stale device and log the
/// decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Displaced {
    pub(crate) identity: IdentityHash,
    pub(crate) addr: Addr,
    pub(crate) role: Role,
    pub(crate) silence_ms: u64,
}

/// One live, handshaked link.
pub(crate) struct Link {
    pub(crate) identity: IdentityHash,
    pub(crate) addr: Addr,
    pub(crate) role: Role,
    /// Negotiated ATT MTU for this link, updated when the carrier reports
    /// a newer value.
    pub(crate) mtu: usize,
    defrag: BleDefragmenter,
    /// When this link last delivered ANY frame, keepalives included —
    /// the liveness clock both [`LinkTable::expire`] and the duplicate
    /// rule read (#382).
    last_heard_ms: u64,
    last_keepalive_tx_ms: u64,
}

/// Why a link was (not) admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    Accept,
    /// The peer presented our own identity — we connected to ourselves
    /// through some reflective path. Firmware: `BLE_LINK_SELF`.
    RejectSelf,
    /// A dial of OURS reached an identity that already holds a live
    /// link, which keeps the peer. Firmware:
    /// `BLE_LINK_DUP … action=refuse`. `old_silence_ms` is how long the
    /// old link had delivered nothing at all: reported so a capture can
    /// check the decision, never consulted in making it (#382).
    RejectDuplicate {
        old_silence_ms: u64,
    },
    /// `max_links` reached.
    RejectFull,
}

/// What one inbound carrier frame amounted to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Inbound {
    /// Sub-header-size frame: keepalive (or noise). Filtered before
    /// reassembly, per the firmware's rule (columba.rs: anything shorter
    /// than the 5-byte fragment header is ignored); the reference's
    /// explicit `len == 1 && data[0] == 0x00` check is a subset.
    Keepalive,
    /// Fragment consumed, packet not yet complete.
    NeedMore,
    /// A whole Reticulum packet, reassembled.
    Packet(Vec<u8>),
    /// A central completed the 16-byte identity handshake and the link is
    /// now live. The driver should log its `BLE_LINK_UP` — and, when the
    /// admission displaced an old link with the same identity,
    /// disconnect the displaced device.
    HandshakeComplete {
        identity: IdentityHash,
        displaced: Option<Displaced>,
    },
    /// A central's handshake was rejected; the driver must disconnect the
    /// device. The old link, if the rejection displaced nothing, stays.
    HandshakeRejected(Admission),
    /// Frame from an address with no link and no valid handshake — a peer
    /// writing data before identifying itself. Ignored.
    NotHandshaked,
    /// The defragmenter rejected the frame.
    Error,
}

/// A peripheral-role connection that has not handshaked yet.
#[derive(Debug)]
struct Pending {
    addr: Addr,
    since_ms: u64,
}

/// Links and pending handshakes torn down by [`LinkTable::expire`]. The
/// driver disconnects the named devices; the table has already forgotten
/// them.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Expired {
    /// `(identity, addr, role)` of each timed-out link.
    pub(crate) links: Vec<(IdentityHash, Addr, Role)>,
    /// Addresses whose handshake never arrived.
    pub(crate) pending: Vec<Addr>,
}

/// What the core's #376 delivery hint made of one outbound packet.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub(crate) enum TxRoute {
    /// No hint — a broadcast (announce, path request, anything the core
    /// did not address at a named peer). Every live link, as always.
    #[default]
    Flood,
    /// Hinted, and the peer holds a live link: that link alone.
    Routed { peer: IdentityHash, role: Role },
    /// Hinted at a peer with NO live link here. Nothing is sent: see
    /// [`LinkTable::plan_tx_to`] for why this is a drop and not a
    /// fallback flood.
    NoLink { peer: IdentityHash },
}

/// The outbound fan-out for one Reticulum packet: what to write to the
/// shared notify pipe, and what to write to each central-role link.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TxPlan {
    /// What the hint decided (Codeberg #376) — the driver's log line.
    pub(crate) route: TxRoute,
    /// Fragments for the peripheral-side notify pipe, fragmented at the
    /// minimum MTU across peripheral links. Empty when no peripheral link
    /// is live.
    pub(crate) notify_fragments: Vec<Vec<u8>>,
    /// Per central-role link: the address and its fragments at that
    /// link's own MTU.
    pub(crate) central: Vec<(Addr, Vec<Vec<u8>>)>,
}

/// Which links are due a keepalive.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct KeepalivePlan {
    /// Write one keepalive byte to the notify pipe (it reaches every
    /// subscribed central at once).
    pub(crate) notify: bool,
    /// Central-role links due an RX write.
    pub(crate) central: Vec<Addr>,
}

/// The link table: every live link and every pending peripheral-side
/// handshake, owned by the interface task.
pub(crate) struct LinkTable {
    own_identity: IdentityHash,
    max_links: usize,
    links: Vec<Link>,
    pending: Vec<Pending>,
    /// Reassemblies a link discarded before completion, queued for the
    /// driver's `BLE_RX_ABANDON` lines (#373): `(identity, lost,
    /// running total)` in frame order. Each entry is one or more whole
    /// Reticulum packets this receiver lost — a torn or interleaved
    /// fragment stream from the peer — which before the line existed
    /// was invisible on every surface.
    abandon_reports: Vec<(IdentityHash, u32, u32)>,
}

impl LinkTable {
    pub(crate) fn new(own_identity: IdentityHash, max_links: usize) -> Self {
        Self {
            own_identity,
            max_links: max_links.max(1),
            links: Vec::new(),
            pending: Vec::new(),
            abandon_reports: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn link_count(&self) -> usize {
        self.links.len()
    }

    pub(crate) fn is_full(&self) -> bool {
        self.links.len() >= self.max_links
    }

    /// Whether the advertisement belongs on the air: a full table must
    /// not advertise. The firmware's `ADV_LOCK` policy (columba.rs,
    /// ead0bce): with every slot in a session nothing advertises — a
    /// full node is honestly silent rather than accepting a connection
    /// it would immediately refuse. The driver deregisters the
    /// advertisement when this flips false and re-registers it when a
    /// slot frees; the GATT application stays up throughout, because
    /// live peripheral sessions keep using it (as the firmware keeps
    /// serving its sessions while dark).
    pub(crate) fn should_advertise(&self) -> bool {
        !self.is_full()
    }

    pub(crate) fn link_by_addr(&self, addr: &Addr) -> Option<&Link> {
        self.links.iter().find(|l| &l.addr == addr)
    }

    fn link_mut_by_addr(&mut self, addr: &Addr) -> Option<&mut Link> {
        self.links.iter_mut().find(|l| &l.addr == addr)
    }

    /// Whether a scanner hit at this address already belongs to a live
    /// link or a pending handshake (then it must not be dialled again).
    pub(crate) fn knows_addr(&self, addr: &Addr) -> bool {
        self.links.iter().any(|l| &l.addr == addr) || self.pending.iter().any(|p| &p.addr == addr)
    }

    /// Whether any peripheral-side handshake is pending — the table's
    /// half of the fallback clock's busy input (#375 part 3, the eager
    /// spec): a connection that has not identified yet holds the clock,
    /// a handshaked link does not. A linked peer's address is kept out
    /// of the window by [`Self::knows_addr`] instead, so the clock may
    /// run — and the fallback fire — while links are live.
    pub(crate) fn has_pending_handshakes(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Whether `identity` still owns a live link (Codeberg #365).
    ///
    /// Decides whether a link removal is a real peer loss the transport
    /// must hear about (drop the paths via that peer) or a same-identity
    /// churn — a link displaced by the peer's own reconnect — where the peer
    /// is still reachable and the paths must stay.
    pub(crate) fn knows_identity(&self, identity: &IdentityHash) -> bool {
        self.links.iter().any(|l| &l.identity == identity)
    }

    /// Admission check + insert, shared by both roles.
    ///
    /// Duplicate handling is identity-keyed, never address-keyed, because
    /// addresses rotate (v2.2 §"Why Not Use MAC Addresses as Keys?").
    /// Which of the two links the peer keeps is decided by
    /// [`judge_duplicate`] from `role` and from nothing else —
    /// `Peripheral` means the peer dialled US and displaces, `Central`
    /// means we dialled it and are refused, and the firmware registry
    /// applies the identical function to the identical input (#382).
    /// The old link's silence is measured for the log line on both
    /// paths and consulted on neither; a link that has really stopped
    /// answering is [`LinkTable::expire`]'s business, and it removes it
    /// whether or not anybody dials the identity. A displaced link is
    /// returned as [`Displaced`] so the driver can disconnect the stale
    /// device and log the decision.
    pub(crate) fn admit(
        &mut self,
        identity: IdentityHash,
        addr: Addr,
        role: Role,
        mtu: usize,
        now_ms: u64,
    ) -> (Admission, Option<Displaced>) {
        if identity == self.own_identity {
            return (Admission::RejectSelf, None);
        }
        let mut displaced = None;
        if let Some(pos) = self.links.iter().position(|l| l.identity == identity) {
            let silence_ms = now_ms.saturating_sub(self.links[pos].last_heard_ms);
            if judge_duplicate(role.origin()) == Duplicate::Refuse {
                return (
                    Admission::RejectDuplicate {
                        old_silence_ms: silence_ms,
                    },
                    None,
                );
            }
            let old = self.links.remove(pos);
            displaced = Some(Displaced {
                identity: old.identity,
                addr: old.addr,
                role: old.role,
                silence_ms,
            });
        }
        if self.links.len() >= self.max_links {
            return (Admission::RejectFull, displaced);
        }
        self.links.push(Link {
            identity,
            addr,
            role,
            mtu: mtu.max(MIN_MTU),
            defrag: BleDefragmenter::new(),
            last_heard_ms: now_ms,
            last_keepalive_tx_ms: now_ms,
        });
        (Admission::Accept, displaced)
    }

    /// Remove the link at `addr` (carrier reported it down). Returns what
    /// was removed, for the `BLE_LINK_DOWN` log line.
    pub(crate) fn remove_by_addr(&mut self, addr: &Addr) -> Option<(IdentityHash, Addr, Role)> {
        self.pending.retain(|p| &p.addr != addr);
        let pos = self.links.iter().position(|l| &l.addr == addr)?;
        let old = self.links.remove(pos);
        Some((old.identity, old.addr, old.role))
    }

    /// A frame written to our RX characteristic (we are the peripheral).
    ///
    /// `mtu` is the exchanged ATT MTU BlueZ reports with the write; it
    /// keeps the link's fragment sizing current. The first 16-byte write
    /// from an unknown address is the identity handshake
    /// (BLE_PROTOCOL_v2.2 §Identity Handshake); after the handshake a
    /// 16-byte frame is ordinary fragment traffic, the firmware's
    /// rig-proven reading (columba.rs gates on `!handshake_done`) — the
    /// reference consumes *every* 16-byte frame from a known address,
    /// which would eat a real 16-byte tail fragment at small MTUs.
    pub(crate) fn peripheral_frame(
        &mut self,
        addr: Addr,
        mtu: usize,
        data: &[u8],
        now_ms: u64,
    ) -> Inbound {
        if self.link_by_addr(&addr).is_some() {
            if let Some(link) = self.link_mut_by_addr(&addr) {
                if mtu >= MIN_MTU {
                    link.mtu = mtu;
                }
            }
            return self.link_frame(addr, data, now_ms);
        }
        // No live link at this address: only a handshake opens one.
        if data.len() == 16 {
            let mut identity = [0u8; 16];
            identity.copy_from_slice(data);
            self.pending.retain(|p| p.addr != addr);
            let (admission, displaced) = self.admit(identity, addr, Role::Peripheral, mtu, now_ms);
            return match admission {
                Admission::Accept => Inbound::HandshakeComplete {
                    identity,
                    displaced,
                },
                other => Inbound::HandshakeRejected(other),
            };
        }
        // Track the connection so a peer that never identifies itself is
        // eventually disconnected instead of camping for free.
        if !self.pending.iter().any(|p| p.addr == addr) {
            self.pending.push(Pending {
                addr,
                since_ms: now_ms,
            });
        }
        if data.len() < FRAGMENT_HEADER_SIZE {
            Inbound::Keepalive
        } else {
            Inbound::NotHandshaked
        }
    }

    /// A notification from a peer we are connected to as central.
    pub(crate) fn central_frame(&mut self, addr: Addr, data: &[u8], now_ms: u64) -> Inbound {
        if self.link_by_addr(&addr).is_none() {
            return Inbound::NotHandshaked;
        }
        self.link_frame(addr, data, now_ms)
    }

    fn link_frame(&mut self, addr: Addr, data: &[u8], now_ms: u64) -> Inbound {
        let Some(link) = self.link_mut_by_addr(&addr) else {
            return Inbound::NotHandshaked;
        };
        // Every inbound frame is evidence the peer is there, keepalives
        // included: a quiet phone sends nothing else for minutes on end
        // (#382). This clock is the expiry sweep's — the duplicate rule
        // reports it and no longer reads it.
        link.last_heard_ms = now_ms;
        // Keepalives are still filtered before reassembly.
        if data.len() < FRAGMENT_HEADER_SIZE {
            return Inbound::Keepalive;
        }
        let before = link.defrag.abandoned_count();
        let result = link.defrag.process(data, now_ms);
        if matches!(result, DefragResult::Error) {
            // Hard reset, as the firmware does: a garbage frame amid a
            // reassembly must not leave a stale head for the next
            // packet's tail to complete (#255) — and the head it
            // discards is a loss this link must report.
            link.defrag.abandon();
        }
        let after = link.defrag.abandoned_count();
        let report =
            (after != before).then(|| (link.identity, after.saturating_sub(before), after));
        if let Some(report) = report {
            self.abandon_reports.push(report);
        }
        match result {
            DefragResult::Complete(packet) => Inbound::Packet(packet),
            DefragResult::NeedMore => Inbound::NeedMore,
            DefragResult::Error => Inbound::Error,
        }
    }

    /// Drain the queued reassembly-loss reports (#373); the driver turns
    /// each into one `BLE_RX_ABANDON` line.
    pub(crate) fn take_abandon_reports(&mut self) -> Vec<(IdentityHash, u32, u32)> {
        std::mem::take(&mut self.abandon_reports)
    }

    /// Fan one outbound Reticulum packet out to every live link — the
    /// no-hint case of [`Self::plan_tx_to`], kept for the tests that pin
    /// the broadcast fan-out on its own.
    #[cfg(test)]
    pub(crate) fn plan_tx(&self, packet: &[u8]) -> TxPlan {
        self.plan_tx_to(packet, None)
    }

    /// Plan one outbound packet, honouring the core's #376 delivery hint.
    ///
    /// `peer: None` is the broadcast case and fans out to every live link,
    /// exactly as before. `peer: Some(id)` names the ONE peer the core
    /// addressed the packet at, and the plan then carries that peer's link
    /// alone: a routed packet copied onto the other links costs their
    /// airtime, and — the 2026-09-09 desk failure — the neighbour that
    /// received the stray copy forwards it back to the addressee, which
    /// then counts the duplicate.
    ///
    /// Two properties of this carrier shape the result:
    ///
    /// - A peer holding several links is still one peer. Any of its links
    ///   reaches it, so the first one found is taken (the registry churns
    ///   through two links only while a rotated-address reconnect displaces
    ///   its predecessor).
    /// - The peripheral side has ONE notify characteristic and BlueZ fans a
    ///   notification out to every subscribed central. Routing to a
    ///   peripheral-role peer therefore still reaches the other subscribed
    ///   centrals; what it does drop is every CENTRAL-role link, which is
    ///   where the board-to-board copy in the field went. Targeting one
    ///   subscriber would need a characteristic per link, which the Columba
    ///   wire protocol does not have.
    /// - A hint naming a peer with no live link DROPS the packet rather
    ///   than falling back to a flood. The hint exists because the core
    ///   routed these bytes at that neighbour; the remaining links are not
    ///   a route to it, and flooding them re-creates exactly the relayed
    ///   duplicate this change removes. The peer's disappearance is already
    ///   reported to the core as a peer loss (Codeberg #365), which culls
    ///   the paths via it, so the next packet is routed afresh instead of
    ///   sprayed.
    pub(crate) fn plan_tx_to(&self, packet: &[u8], peer: Option<&IdentityHash>) -> TxPlan {
        let mut plan = TxPlan::default();
        if let Some(peer) = peer {
            let Some(link) = self.links.iter().find(|l| &l.identity == peer) else {
                plan.route = TxRoute::NoLink { peer: *peer };
                return plan;
            };
            plan.route = TxRoute::Routed {
                peer: *peer,
                role: link.role,
            };
            match link.role {
                // Fragmented at the MINIMUM peripheral MTU, not this
                // link's: the notification is shared, so a fragment sized
                // for this peer would be truncated at a smaller-MTU
                // subscriber. Same rule as the flood path below.
                Role::Peripheral => {
                    let mtu = self
                        .links
                        .iter()
                        .filter(|l| l.role == Role::Peripheral)
                        .map(|l| l.mtu)
                        .min()
                        .unwrap_or(link.mtu);
                    plan.notify_fragments = fragment_packet(packet, mtu);
                }
                Role::Central => plan
                    .central
                    .push((link.addr, fragment_packet(packet, link.mtu))),
            }
            return plan;
        }
        let periph_mtu = self
            .links
            .iter()
            .filter(|l| l.role == Role::Peripheral)
            .map(|l| l.mtu)
            .min();
        if let Some(mtu) = periph_mtu {
            plan.notify_fragments = fragment_packet(packet, mtu);
        }
        for link in self.links.iter().filter(|l| l.role == Role::Central) {
            plan.central
                .push((link.addr, fragment_packet(packet, link.mtu)));
        }
        plan
    }

    /// Live links, for the driver's `BLE_TX_FLOOD links=` count.
    pub(crate) fn live_links(&self) -> usize {
        self.links.len()
    }

    /// Which links are due a keepalive at `now_ms`; marks them sent.
    ///
    /// The notify pipe reaches every subscribed central at once, so one
    /// due peripheral link triggers one shared keepalive and rearms all
    /// of them.
    pub(crate) fn keepalives_due(&mut self, now_ms: u64) -> KeepalivePlan {
        let mut plan = KeepalivePlan::default();
        let periph_due = self.links.iter().any(|l| {
            l.role == Role::Peripheral
                && now_ms.saturating_sub(l.last_keepalive_tx_ms) >= KEEPALIVE_INTERVAL_MS
        });
        for link in &mut self.links {
            match link.role {
                Role::Peripheral if periph_due => {
                    link.last_keepalive_tx_ms = now_ms;
                }
                Role::Central
                    if now_ms.saturating_sub(link.last_keepalive_tx_ms)
                        >= KEEPALIVE_INTERVAL_MS =>
                {
                    link.last_keepalive_tx_ms = now_ms;
                    plan.central.push(link.addr);
                }
                _ => {}
            }
        }
        plan.notify = periph_due;
        plan
    }

    /// Tear down silent links and overdue handshakes.
    pub(crate) fn expire(&mut self, now_ms: u64) -> Expired {
        let mut expired = Expired::default();
        self.links.retain(|l| {
            if now_ms.saturating_sub(l.last_heard_ms) >= LINK_TIMEOUT_MS {
                expired.links.push((l.identity, l.addr, l.role));
                false
            } else {
                true
            }
        });
        self.pending.retain(|p| {
            if now_ms.saturating_sub(p.since_ms) >= HANDSHAKE_TIMEOUT_MS {
                expired.pending.push(p.addr);
                false
            } else {
                true
            }
        });
        expired
    }
}

// ---------------------------------------------------------------------
// Scan decision — the shared parser and rule on BlueZ-shaped inputs
// ---------------------------------------------------------------------

/// The Columba service UUID, `37145b00-442d-4a94-917f-8f42c5da28e3`.
pub(crate) const SERVICE_UUID_U128: u128 = 0x37145b00_442d_4a94_917f_8f42c5da28e3;

/// The same UUID in AD-structure byte order (little-endian), as
/// `parse_peer_advertisement` compares it.
pub(crate) const SERVICE_UUID_LE: [u8; 16] = SERVICE_UUID_U128.to_le_bytes();

/// The advertised device name, `LN-<hex8>` — the firmware's own
/// derivation (`leviculum_ble_tx::device_name`) over the daemon identity,
/// so a scanner listing shows lnsd exactly like a board.
pub(crate) fn local_name(identity: &IdentityHash) -> String {
    String::from_utf8_lossy(&leviculum_ble_tx::device_name(identity)).into_owned()
}

/// A BLE address in display order as the number the v2.2 sort compares.
/// `leviculum_ble_tx::addr_value` takes the wire (LSB-first) order; a
/// `bluer::Address` is the displayed order, so it is reversed here once.
pub(crate) fn addr_value_display(addr: &Addr) -> u64 {
    let mut le = *addr;
    le.reverse();
    addr_value(&le)
}

/// What one scanner sighting resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScanDecision {
    pub(crate) decision: ConnectDecision,
    /// Whether the peer carried a readable v0.3.0 capability record
    /// (`caps_record=` in the `BLE_SCAN_DECISION` line; when 0, `caps`
    /// is meaningless).
    pub(crate) caps_record: bool,
    pub(crate) caps: u8,
}

/// Decide who connects, from the properties BlueZ hands a scanner.
///
/// BlueZ parses the advertisement for us — the service list into a UUID
/// set, the manufacturer record into a `CID → payload` map — so the raw
/// PDU the shared parser wants is rebuilt from those parts and run
/// through `parse_peer_advertisement` + `should_initiate` unchanged.
/// Rebuilding costs a few bytes on the stack and buys the exact
/// rig-proven version gate and decision table instead of a second
/// reading of them.
///
/// Returns `None` when the sighting does not offer the Columba service
/// (not a peer; no decision to log).
///
/// `mode` is the #375 fallback switch, fed from the [`ScanScheduler`]'s
/// clock — the same clock the firmware's central task runs, so the
/// periculum `ble_room` cell (#49) exercises the rule the boards
/// actually run.
pub(crate) fn decide_from_scan(
    local_addr: &Addr,
    peer_addr: &Addr,
    offers_service: bool,
    manufacturer_ffff: Option<&[u8]>,
    mode: ScanMode,
) -> Option<ScanDecision> {
    let mut pdu: Vec<u8> = Vec::with_capacity(2 + 16 + 2 + MANUFACTURER_DATA_LEN + 2);
    if offers_service {
        pdu.push(17); // 1 type byte + 16 UUID bytes
        pdu.push(0x07); // Complete List of 128-bit Service Class UUIDs
        pdu.extend_from_slice(&SERVICE_UUID_LE);
    }
    if let Some(payload) = manufacturer_ffff {
        // BlueZ strips the company ID into the map key; the AD structure
        // carries it in front of the payload.
        let data_len = 2 + payload.len();
        if let Ok(len_byte) = u8::try_from(1 + data_len) {
            pdu.push(len_byte);
            pdu.push(0xFF); // Manufacturer Specific Data
            pdu.extend_from_slice(&0xFFFFu16.to_le_bytes());
            pdu.extend_from_slice(payload);
        }
    }
    let parsed = parse_peer_advertisement(&pdu, &SERVICE_UUID_LE);
    if !parsed.offers_service {
        return None;
    }
    let decision = should_initiate(
        LOCAL_CAPS,
        addr_value_display(local_addr),
        parsed.caps,
        addr_value_display(peer_addr),
        mode,
    );
    Some(ScanDecision {
        decision,
        caps_record: parsed.caps.is_some(),
        caps: parsed.caps.unwrap_or(0),
    })
}

// ---------------------------------------------------------------------
// The fallback clock and the collection window (#375 part 2, item 3)
// ---------------------------------------------------------------------

/// The firmware central task's fallback clock and scan window, as one
/// pure state machine for lnsd's driver loop: no tokio, no BlueZ, the
/// caller passes `now_ms` and performs what the return values ask for.
///
/// Same policy, same shared pieces as the boards: the clock switches
/// [`decide_from_scan`] to [`ScanMode::Fallback`] after
/// [`SCAN_FALLBACK_AFTER_MS`] without a dial or a connection — held at
/// zero only while a dial of ours is in flight or a handshake is
/// pending (the eager spec, #375 part 3: live links no longer suspend
/// it, their addresses are simply never dialled) — and eligible
/// sightings collect for [`SCAN_WINDOW_COLLECT_MS`] into the firmware's
/// own [`CandidateTable`], so which peer gets dialled is decided by the
/// identical code on both stacks.
pub(crate) struct ScanScheduler {
    /// When the current strict phase began.
    strict_since_ms: u64,
    /// Whether this phase's strict-to-fallback switch was already
    /// announced (`BLE_SCAN_FALLBACK` is logged once per phase).
    announced: bool,
    /// The open collection window: close deadline and candidates.
    window: Option<(u64, CandidateTable<Addr, WINDOW_CANDIDATES>)>,
}

impl ScanScheduler {
    pub(crate) fn new(now_ms: u64) -> Self {
        Self {
            strict_since_ms: now_ms,
            announced: false,
            window: None,
        }
    }

    /// The [`ScanMode`] for a sighting at `now_ms`. `busy` means a dial
    /// of ours is in flight or a handshake is pending (either role) —
    /// NOT "any link" (#375 part 3): while true the clock is held at
    /// zero, because the in-flight attempt's outcome is about to reset
    /// it anyway. The second value is the elapsed time to log as
    /// `BLE_SCAN_FALLBACK`, returned exactly once per strict phase that
    /// reaches the bound.
    pub(crate) fn mode(&mut self, busy: bool, now_ms: u64) -> (ScanMode, Option<u64>) {
        if busy {
            self.note_reset(now_ms);
            return (ScanMode::Strict, None);
        }
        let elapsed = now_ms.saturating_sub(self.strict_since_ms);
        if elapsed < SCAN_FALLBACK_AFTER_MS {
            return (ScanMode::Strict, None);
        }
        let announce = (!self.announced).then_some(elapsed);
        self.announced = true;
        (ScanMode::Fallback, announce)
    }

    /// A connection formed or ended: the strict phase starts over, as
    /// the firmware resets its clock at every connection event and
    /// teardown (#375 §0 — resetting only at identity left a gap the
    /// rig's fallback fired into).
    pub(crate) fn note_reset(&mut self, now_ms: u64) {
        self.strict_since_ms = now_ms;
        self.announced = false;
    }

    /// An eligible sighting — initiate verdict, every driver filter
    /// passed. The first one opens the window; all of them collect.
    pub(crate) fn offer(&mut self, addr: Addr, decision: ConnectDecision, now_ms: u64) {
        let (_, table) = self
            .window
            .get_or_insert_with(|| (now_ms + SCAN_WINDOW_COLLECT_MS, CandidateTable::new()));
        table.offer(addr_value_display(&addr), decision, addr);
    }

    /// Close the window once its bound has passed: the dial target —
    /// lowest eligible address, strict verdicts before fallback
    /// verdicts — with its rule and the window's `seen` count. A
    /// strict-class choice restarts the strict phase (the rule works,
    /// the fallback clock starts over); a fallback choice does not, so
    /// a failed fallback dial re-enters fallback immediately.
    pub(crate) fn poll(&mut self, now_ms: u64) -> Option<(Addr, ConnectDecision, usize)> {
        if !self
            .window
            .as_ref()
            .is_some_and(|(until, _)| now_ms >= *until)
        {
            return None;
        }
        let (_, table) = self.window.take()?;
        let seen = table.seen();
        let (_, decision, addr) = table.into_best()?;
        if decision != ConnectDecision::InitiateFallback {
            self.note_reset(now_ms);
        }
        Some((addr, decision, seen))
    }
}

// ---------------------------------------------------------------------
// The per-link inter-packet transmit gap (#376)
// ---------------------------------------------------------------------

/// The inter-packet gap lnsd serves on every BLE link: the firmware's
/// compiled default ([`leviculum_ble_tx::DEFAULT_TX_GAP_MS`], 100 ms —
/// the measured desk value, justified on that constant), always. lnsd
/// has no runtime knob, and as the phone stand-in on the rig it must
/// pace exactly as a board does toward a real phone.
///
/// Wraps the firmware's own host-tested [`TxGap`] arithmetic, so "gap"
/// means the same thing on both stacks: measured from the last fragment
/// of one packet to the first fragment of the next on the same link,
/// the first packet never deferred, keepalives neither paced nor
/// sliding the window (they never call
/// [`packet_done`](Self::packet_done)).
pub(crate) struct LinkPacer {
    gap: TxGap,
}

impl LinkPacer {
    pub(crate) fn new() -> Self {
        Self { gap: TxGap::new() }
    }

    /// How long the next packet must still wait at `now_ms`, in
    /// milliseconds; 0 is "send now".
    pub(crate) fn wait_ms(&self, now_ms: u64) -> u64 {
        self.gap.wait_ms(now_ms, effective_tx_gap_ms(None))
    }

    /// The last fragment of a packet was handed to the carrier; the
    /// next packet's gap is measured from here.
    pub(crate) fn packet_done(&mut self, now_ms: u64) {
        self.gap.packet_done(now_ms);
    }
}

// ---------------------------------------------------------------------
// The dial queue — one connection setup in flight, jittered (#49 part 3)
// ---------------------------------------------------------------------

/// Pre-connect jitter bounds. Every dial waits a uniform random delay in
/// `[DIAL_JITTER_MIN_MS, DIAL_JITTER_MAX_MS]` from the moment it becomes
/// startable (head of the queue, no setup in flight).
///
/// The jitter's job is to spread N centrals that elected the same peer
/// out of the same sighting: their collection windows
/// (`SCAN_WINDOW_COLLECT_MS`, 3 s) close within one window length of
/// each other, so a spread up to ~2/3 of a window decorrelates the
/// connect attempts arriving at that peripheral instead of stacking them
/// into one connection event — the overlap btvirt mishandles (periculum
/// #49) and a real controller serves slower. The floor keeps the delay
/// from degenerating to "immediately" for everyone; the ceiling stays an
/// order of magnitude under the 20 s central setup budget
/// (`bluez::SETUP_TIMEOUT`), so a serialised queue still drains several
/// dials per timeout period, and far under the 30 s fallback clock
/// (`SCAN_FALLBACK_AFTER_MS`), which the queued attempt holds at zero
/// while it waits.
pub(crate) const DIAL_JITTER_MIN_MS: u64 = 250;
/// Upper jitter bound; see [`DIAL_JITTER_MIN_MS`].
pub(crate) const DIAL_JITTER_MAX_MS: u64 = 2_000;

/// What [`DialQueue::enqueue`] admitted, for the driver's
/// `BLE_DIAL_QUEUE` line: how many dials now wait (this one included)
/// and the jitter this dial will serve before it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DialQueued {
    pub(crate) depth: usize,
    pub(crate) wait_ms: u64,
}

/// One deferred dial.
#[derive(Debug)]
struct QueuedDial {
    addr: Addr,
    decision: ConnectDecision,
    /// Drawn at enqueue (so the driver can log it), served from the
    /// moment the dial becomes startable.
    jitter_ms: u64,
    /// Absolute start time, armed when the dial reaches the head of the
    /// queue with no setup in flight. `None` = still blocked.
    deadline_ms: Option<u64>,
}

/// Serialises central-role connection setups: at most ONE in flight per
/// adapter — from the dial until the handshake completes or the setup
/// times out — with a bounded random pre-connect jitter before every
/// dial. Media-aware interface behaviour: BLE connection setup is the
/// carrier's most collision-sensitive phase (a peripheral serving
/// overlapping incoming connections is exactly where btvirt phantoms and
/// where a real controller's throughput dies), and the firmware's
/// SoftDevice enforces one `LE Create Connection` at a time in hardware
/// — this queue is the same policy for a BlueZ host. Pure state machine:
/// the caller passes `now_ms`, spawns the dial [`Self::pop_ready`] hands
/// back, and reports the setup's end via [`Self::release`].
///
/// No TTL on queued dials: the driver re-checks eligibility (table,
/// backoff, known addresses) when a dial pops, and a stale entry it
/// drops releases the slot to the next one, so the queue cannot wedge.
pub(crate) struct DialQueue {
    rng: Xorshift64,
    in_flight: Option<Addr>,
    queue: Vec<QueuedDial>,
}

impl DialQueue {
    pub(crate) fn new(seed: u64) -> Self {
        Self {
            rng: Xorshift64::new(seed),
            in_flight: None,
            queue: Vec::new(),
        }
    }

    /// Whether `addr` is already queued or being set up (then a sighting
    /// of it must not be dialled again — the same contract as
    /// [`LinkTable::knows_addr`]).
    pub(crate) fn knows(&self, addr: &Addr) -> bool {
        self.in_flight.as_ref() == Some(addr) || self.queue.iter().any(|d| &d.addr == addr)
    }

    /// Whether nothing is queued or in flight — the queue's half of the
    /// fallback clock's busy input: a deferred dial's outcome is about
    /// to reset the clock exactly like an in-flight one's (#375 part 3).
    pub(crate) fn is_idle(&self) -> bool {
        self.in_flight.is_none() && self.queue.is_empty()
    }

    /// Defer one elected dial. Returns what to log, or `None` when the
    /// address is already queued or in flight (nothing changed).
    pub(crate) fn enqueue(
        &mut self,
        addr: Addr,
        decision: ConnectDecision,
        now_ms: u64,
    ) -> Option<DialQueued> {
        if self.knows(&addr) {
            return None;
        }
        let jitter_ms =
            DIAL_JITTER_MIN_MS + self.rng.next() % (DIAL_JITTER_MAX_MS - DIAL_JITTER_MIN_MS + 1);
        self.queue.push(QueuedDial {
            addr,
            decision,
            jitter_ms,
            deadline_ms: None,
        });
        self.arm_head(now_ms);
        Some(DialQueued {
            depth: self.queue.len(),
            wait_ms: jitter_ms,
        })
    }

    /// The dial to start now, if its jitter has been served and no setup
    /// is in flight. The returned address IS the in-flight setup from
    /// here on; the caller either spawns the connection or, when the
    /// re-check refuses it, calls [`Self::release`] so the next dial can
    /// arm.
    pub(crate) fn pop_ready(&mut self, now_ms: u64) -> Option<(Addr, ConnectDecision)> {
        if self.in_flight.is_some() {
            return None;
        }
        let ready = self
            .queue
            .first()
            .is_some_and(|d| d.deadline_ms.is_some_and(|deadline| now_ms >= deadline));
        if !ready {
            return None;
        }
        let dial = self.queue.remove(0);
        self.in_flight = Some(dial.addr);
        Some((dial.addr, dial.decision))
    }

    /// The setup at `addr` ended — handshake complete, setup timeout,
    /// connect failure, or the driver's re-check refusing the popped
    /// dial. Frees the slot and starts the next queued dial's jitter.
    /// Idempotent: releases of addresses not in flight (e.g. the
    /// `CentralGone` that follows a completed handshake's session end)
    /// change nothing.
    pub(crate) fn release(&mut self, addr: &Addr, now_ms: u64) {
        if self.in_flight.as_ref() == Some(addr) {
            self.in_flight = None;
            self.arm_head(now_ms);
        }
    }

    /// When the head dial wants to start, for the driver's timer. `None`
    /// while a setup is in flight or nothing is queued.
    pub(crate) fn next_deadline_ms(&self) -> Option<u64> {
        if self.in_flight.is_some() {
            return None;
        }
        self.queue.first().and_then(|d| d.deadline_ms)
    }

    /// Start the head dial's jitter if it is startable and not armed yet.
    fn arm_head(&mut self, now_ms: u64) {
        if self.in_flight.is_some() {
            return;
        }
        if let Some(head) = self.queue.first_mut() {
            if head.deadline_ms.is_none() {
                head.deadline_ms = Some(now_ms + head.jitter_ms);
            }
        }
    }
}

/// Fast non-cryptographic PRNG (xorshift64), the tcp.rs helper with a
/// caller-provided seed so tests are deterministic.
struct Xorshift64(u64);

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self(seed | 1) // xorshift has no escape from the all-zero state
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviculum_core::framing::ble::payload_per_fragment;

    const ID_A: IdentityHash = [0xA1; 16];
    const ID_B: IdentityHash = [0xB2; 16];
    const OWN: IdentityHash = [0x0E; 16];
    const ADDR_1: Addr = [0xC0, 0x00, 0x00, 0x00, 0x00, 0x01];
    const ADDR_2: Addr = [0xC0, 0x00, 0x00, 0x00, 0x00, 0x02];
    const ADDR_3: Addr = [0xC0, 0x00, 0x00, 0x00, 0x00, 0x03];

    fn table() -> LinkTable {
        LinkTable::new(OWN, DEFAULT_MAX_LINKS)
    }

    /// lnsd paces every BLE link at the firmware's compiled default gap
    /// (#376): 100 ms between packets on a link, no knob, first packet
    /// free, the boundary itself send-now. The pacer wraps the
    /// firmware's `TxGap`, so this pins that lnsd resolves the SAME
    /// default the boards' pumps do.
    #[test]
    fn lnsd_paces_links_with_the_firmware_default_gap() {
        let default = u64::from(leviculum_ble_tx::DEFAULT_TX_GAP_MS);
        let mut pacer = LinkPacer::new();
        assert_eq!(pacer.wait_ms(0), 0, "first packet is never deferred");
        pacer.packet_done(1_000);
        assert_eq!(pacer.wait_ms(1_000), default, "back to back: full gap");
        assert_eq!(pacer.wait_ms(1_040), default - 40, "the remainder");
        assert_eq!(pacer.wait_ms(1_000 + default), 0, "boundary is send-now");
        // A keepalive never calls packet_done, so it cannot slide the
        // window — the pacer only ever hears about packets.
        assert_eq!(pacer.wait_ms(1_050), default - 50);
    }

    /// lnsd advertises under the same name a board with the same identity
    /// would: the firmware's `device_name` derivation, byte for byte.
    #[test]
    fn local_name_matches_the_firmware_derivation() {
        let hash: IdentityHash = [
            0xa1, 0xb2, 0xc3, 0xd4, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99, 0x88, 0x77, 0x66,
            0x55, 0x44,
        ];
        assert_eq!(local_name(&hash), "LN-a1b2c3d4");
        assert_eq!(
            local_name(&hash).as_bytes(),
            leviculum_ble_tx::device_name(&hash)
        );
    }

    /// The peer-loss report decision (Codeberg #365): a removal that
    /// leaves the identity without any live link is a real loss; a link
    /// displaced by the peer's own reconnect is not.
    #[test]
    fn knows_identity_separates_peer_loss_from_same_identity_churn() {
        let mut t = table();
        let (adm, _) = t.admit(ID_A, ADDR_1, Role::Peripheral, 100, 0);
        assert_eq!(adm, Admission::Accept);
        assert!(t.knows_identity(&ID_A));
        assert!(!t.knows_identity(&ID_B));

        // Same identity dials in again from a fresh address: displaced,
        // but the peer is still linked — no peer-loss report.
        let (adm, displaced) = t.admit(ID_A, ADDR_2, Role::Peripheral, 100, 1);
        assert_eq!(adm, Admission::Accept);
        assert_eq!(
            displaced.map(|d| (d.identity, d.addr)),
            Some((ID_A, ADDR_1))
        );
        assert!(
            t.knows_identity(&ID_A),
            "displacement is churn, not loss: the identity still owns a link"
        );

        // The real loss: the only link goes away.
        let removed = t.remove_by_addr(&ADDR_2);
        assert_eq!(removed.map(|(id, ..)| id), Some(ID_A));
        assert!(
            !t.knows_identity(&ID_A),
            "after the last link is gone the loss must be reportable"
        );
    }

    /// The #373 receiver-side visibility: a torn/interleaved fragment
    /// stream that costs a partial packet queues one report the driver
    /// turns into a `BLE_RX_ABANDON` line, and the second packet still
    /// reassembles. Before the report existed the discard was a silent
    /// `NeedMore`.
    #[test]
    fn an_abandoned_reassembly_is_reported_and_the_next_packet_survives() {
        let mut t = table();
        let (adm, _) = t.admit(ID_A, ADDR_1, Role::Central, 185, 0);
        assert_eq!(adm, Admission::Accept);

        let a = vec![0xAA; 300];
        let b = vec![0xBB; 300];
        let frags_a = fragment_packet(&a, 185);
        let frags_b = fragment_packet(&b, 185);
        assert_eq!(frags_a.len(), 2);

        // A's head, then B's whole stream: A is discarded, B completes.
        assert_eq!(t.central_frame(ADDR_1, &frags_a[0], 100), Inbound::NeedMore);
        assert!(t.take_abandon_reports().is_empty(), "A is still pending");
        assert_eq!(t.central_frame(ADDR_1, &frags_b[0], 101), Inbound::NeedMore);
        assert_eq!(
            t.take_abandon_reports(),
            vec![(ID_A, 1, 1)],
            "the torn head is one reported loss"
        );
        assert_eq!(
            t.central_frame(ADDR_1, &frags_b[1], 102),
            Inbound::Packet(b)
        );
        assert!(
            t.take_abandon_reports().is_empty(),
            "completion never counts"
        );
    }

    /// The peer-up report decision (Codeberg #365), the mirror of the
    /// loss side above: `admit` displaces same-identity links only, so
    /// an `Accept` with `displaced == None` is exactly the moment an
    /// identity gains its FIRST link — `knows_identity` flips true and
    /// the orchestrator starts counting the link as known. That is the
    /// condition the interface reports peer-up on; an `Accept` that
    /// carries a displacement is the phone's random-address relink
    /// (churn), and reporting it would spray one pull per ~60 s.
    #[test]
    fn admit_without_displacement_is_the_peer_up_report_point() {
        let mut t = table();

        assert!(!t.knows_identity(&ID_A));
        let (adm, displaced) = t.admit(ID_A, ADDR_1, Role::Peripheral, 100, 0);
        assert_eq!(adm, Admission::Accept);
        assert!(
            displaced.is_none(),
            "first link: no displacement — this admission is reported as peer-up"
        );
        assert!(
            t.knows_identity(&ID_A),
            "the report point IS the moment the identity becomes known"
        );

        // The relink: same identity, fresh random address, the peer
        // dialling in. Accepted, but with a displacement — churn, no
        // report.
        let (adm, displaced) = t.admit(ID_A, ADDR_2, Role::Peripheral, 100, 1);
        assert_eq!(adm, Admission::Accept);
        assert!(
            displaced.is_some(),
            "same-identity relink carries the displacement — not a peer-up"
        );

        // After a real loss the next admission is a first link again and
        // must be reported, or the flap-recovery pull never fires.
        let _ = t.remove_by_addr(&ADDR_2);
        assert!(!t.knows_identity(&ID_A));
        let (adm, displaced) = t.admit(ID_A, ADDR_1, Role::Central, 100, 2 * LINK_TIMEOUT_MS);
        assert_eq!(adm, Admission::Accept);
        assert!(
            displaced.is_none(),
            "reconnect after a loss is a first link again: peer-up reported"
        );
    }

    /// The v2.2 §Connection Direction worked example, through the
    /// display-order conversion: `B8:27:EB:A8:A7:22` reads as
    /// 0xB827EBA8A722 and the lower Pi initiates.
    #[test]
    fn addr_value_display_reads_like_the_spec_reads_the_hex_string() {
        let pi1: Addr = [0xB8, 0x27, 0xEB, 0xA8, 0xA7, 0x22];
        let pi2: Addr = [0xB8, 0x27, 0xEB, 0x10, 0x28, 0xCD];
        assert_eq!(addr_value_display(&pi1), 0xB827_EBA8_A722);
        assert_eq!(addr_value_display(&pi2), 0xB827_EB10_28CD);
        assert!(addr_value_display(&pi2) < addr_value_display(&pi1));
    }

    /// The rebuilt-PDU path produces the same decisions as the shared
    /// table: dual-role record, no record, peripheral-only override.
    #[test]
    fn scan_decision_reuses_the_shared_parser_and_rule() {
        let local: Addr = [0x18, 0x69, 0x45, 0x42, 0xAA, 0x0E];
        let higher: Addr = [0xC0, 0x00, 0x00, 0x00, 0x00, 0x01];

        // Dual-role record, our public address sorts below static random.
        let d = decide_from_scan(&local, &higher, true, Some(&[0x03, 0x00]), ScanMode::Strict)
            .expect("service offered");
        assert_eq!(d.decision, ConnectDecision::InitiateLowerAddress);
        assert!(d.caps_record);
        assert_eq!(d.caps, 0x00);

        // No manufacturer record: same sort, caps_record=0 (v2.2 peer).
        let d = decide_from_scan(&local, &higher, true, None, ScanMode::Strict)
            .expect("service offered");
        assert_eq!(d.decision, ConnectDecision::InitiateLowerAddress);
        assert!(!d.caps_record);

        // Peripheral-only override beats a losing sort.
        let lower: Addr = [0x00, 0x00, 0x00, 0x00, 0x00, 0x01];
        let d = decide_from_scan(&local, &lower, true, Some(&[0x03, 0x01]), ScanMode::Strict)
            .expect("service offered");
        assert_eq!(d.decision, ConnectDecision::InitiatePeripheralOnlyPeer);
        assert!(d.decision.initiate());

        // An older record version is ignored → treated as no record.
        let d = decide_from_scan(&local, &lower, true, Some(&[0x02, 0x01]), ScanMode::Strict)
            .expect("service offered");
        assert!(!d.caps_record);
        assert_eq!(d.decision, ConnectDecision::WaitPeerHasLowerAddress);

        // The fallback mode reshapes exactly that losing sort into a
        // dial (#375), same as the firmware's decision table.
        let d = decide_from_scan(
            &local,
            &lower,
            true,
            Some(&[0x02, 0x01]),
            ScanMode::Fallback,
        )
        .expect("service offered");
        assert_eq!(d.decision, ConnectDecision::InitiateFallback);
        assert!(d.decision.initiate());

        // No service, no decision.
        assert_eq!(
            decide_from_scan(&local, &higher, false, None, ScanMode::Strict),
            None
        );
    }

    /// The #375 fallback clock, lnsd edition: strict until the bound,
    /// one announce per phase, and a busy sighting (a dial of ours in
    /// flight or a handshake pending — the eager spec's whole busy set)
    /// holds the clock at zero.
    #[test]
    fn the_fallback_clock_switches_once_per_phase_and_busy_suspends_it() {
        let mut s = ScanScheduler::new(0);
        assert_eq!(s.mode(false, 1_000), (ScanMode::Strict, None));
        assert_eq!(
            s.mode(false, SCAN_FALLBACK_AFTER_MS - 1),
            (ScanMode::Strict, None)
        );
        // The bound: fallback, announced exactly once.
        assert_eq!(
            s.mode(false, SCAN_FALLBACK_AFTER_MS),
            (ScanMode::Fallback, Some(SCAN_FALLBACK_AFTER_MS))
        );
        assert_eq!(
            s.mode(false, SCAN_FALLBACK_AFTER_MS + 5_000),
            (ScanMode::Fallback, None),
            "one announce per phase"
        );

        // A busy sighting (dial in flight / handshake pending)
        // restarts the phase.
        assert_eq!(
            s.mode(true, SCAN_FALLBACK_AFTER_MS + 6_000),
            (ScanMode::Strict, None)
        );
        // The full bound applies again after the busy period ends…
        assert_eq!(
            s.mode(false, 2 * SCAN_FALLBACK_AFTER_MS),
            (ScanMode::Strict, None)
        );
        // …and the next switch announces again.
        let (mode, announce) = s.mode(false, 3 * SCAN_FALLBACK_AFTER_MS);
        assert_eq!(mode, ScanMode::Fallback);
        assert!(announce.is_some(), "a fresh phase re-announces its switch");
    }

    /// The firmware's connection-event reset, lnsd edition (#375 §0):
    /// a link forming or ending restarts the strict phase.
    #[test]
    fn a_connection_event_restarts_the_strict_phase() {
        let mut s = ScanScheduler::new(0);
        assert_eq!(s.mode(false, SCAN_FALLBACK_AFTER_MS).0, ScanMode::Fallback);
        s.note_reset(SCAN_FALLBACK_AFTER_MS + 1_000);
        assert_eq!(
            s.mode(false, SCAN_FALLBACK_AFTER_MS + 2_000).0,
            ScanMode::Strict,
            "the teardown/connect reset grants a fresh strict bound"
        );
    }

    /// The collection window: opens at the first eligible sighting,
    /// closes only after its bound, and elects the lowest eligible
    /// address with strict verdicts ahead of fallback verdicts — via
    /// the shared [`CandidateTable`], so this is the firmware's choice
    /// verbatim.
    #[test]
    fn the_window_collects_and_dials_the_lowest_eligible_candidate() {
        let mut s = ScanScheduler::new(0);
        assert_eq!(s.poll(10_000), None, "no window before a candidate");

        s.offer(ADDR_2, ConnectDecision::InitiateLowerAddress, 1_000);
        assert_eq!(
            s.poll(1_000 + SCAN_WINDOW_COLLECT_MS - 1),
            None,
            "the window holds for its bound"
        );
        // A lower-addressed strict candidate and an even lower fallback
        // candidate arrive during the window.
        s.offer(ADDR_1, ConnectDecision::InitiateLowerAddress, 2_000);
        s.offer(
            [0x00, 0x00, 0x00, 0x00, 0x00, 0x01],
            ConnectDecision::InitiateFallback,
            2_500,
        );
        let (addr, decision, seen) = s
            .poll(1_000 + SCAN_WINDOW_COLLECT_MS)
            .expect("window closes at its bound");
        assert_eq!(addr, ADDR_1, "lowest STRICT candidate wins");
        assert_eq!(decision, ConnectDecision::InitiateLowerAddress);
        assert_eq!(seen, 3);
        assert_eq!(s.poll(60_000), None, "the window is consumed");
    }

    /// The dial-time clock rule, shared with the firmware's central
    /// task: a strict-class choice restarts the strict phase, a
    /// fallback choice must NOT — if that dial fails, the scanner is
    /// still stranded and stays in fallback rather than waiting out
    /// the bound again.
    #[test]
    fn a_fallback_dial_does_not_restart_the_strict_phase() {
        let mut s = ScanScheduler::new(0);
        assert_eq!(s.mode(false, SCAN_FALLBACK_AFTER_MS).0, ScanMode::Fallback);
        s.offer(
            ADDR_1,
            ConnectDecision::InitiateFallback,
            SCAN_FALLBACK_AFTER_MS,
        );
        let closed = SCAN_FALLBACK_AFTER_MS + SCAN_WINDOW_COLLECT_MS;
        assert!(s.poll(closed).is_some());
        assert_eq!(
            s.mode(false, closed + 1).0,
            ScanMode::Fallback,
            "still stranded, still fallback"
        );

        // The strict counterpart restarts the phase.
        s.offer(ADDR_1, ConnectDecision::InitiateLowerAddress, closed + 2);
        let closed2 = closed + 2 + SCAN_WINDOW_COLLECT_MS;
        assert!(s.poll(closed2).is_some());
        assert_eq!(
            s.mode(false, closed2 + 1).0,
            ScanMode::Strict,
            "a strict dial proves the sort works and restarts the clock"
        );
    }

    #[test]
    fn central_handshake_is_admit_then_traffic() {
        let mut t = table();
        let (adm, displaced) = t.admit(ID_A, ADDR_1, Role::Central, 185, 1_000);
        assert_eq!(adm, Admission::Accept);
        assert!(displaced.is_none());
        assert_eq!(t.link_count(), 1);

        // A single-fragment packet round-trips through the link.
        let packet = b"announce bytes".to_vec();
        let frags = fragment_packet(&packet, 185);
        assert_eq!(
            t.central_frame(ADDR_1, &frags[0], 2_000),
            Inbound::Packet(packet)
        );
    }

    #[test]
    fn own_identity_is_rejected_as_self() {
        let mut t = table();
        let (adm, _) = t.admit(OWN, ADDR_1, Role::Central, 185, 0);
        assert_eq!(adm, Admission::RejectSelf);
        assert_eq!(t.link_count(), 0);
    }

    /// The #382 rule through `admit`, both directions on one table:
    /// our own dial loses to a link that still answers, the PEER's dial
    /// wins against the same link.
    #[test]
    fn our_dial_is_refused_where_the_peers_own_dial_displaces() {
        let mut t = table();
        t.admit(ID_A, ADDR_1, Role::Central, 185, 0);
        // Nothing but keepalives for ten minutes — a phone with nothing
        // to report, which is what a healthy idle link looks like.
        let mut now = 0;
        while now < 600_000 {
            now += KEEPALIVE_INTERVAL_MS;
            t.central_frame(ADDR_1, &[0x00], now);
        }

        // Our own dial reaching the same identity under its rotated
        // address: refused, because our dial is evidence of nothing.
        let (adm, displaced) = t.admit(ID_A, ADDR_2, Role::Central, 185, now + 1);
        assert_eq!(
            adm,
            Admission::RejectDuplicate { old_silence_ms: 1 },
            "a keepalive one millisecond ago is a live link"
        );
        assert!(displaced.is_none());
        assert_eq!(t.link_count(), 1);

        // The PEER dialling in with the same identity: displaced, at the
        // same instant and on the same evidence. It has given up on the
        // old link by its own one-link rule.
        let (adm, displaced) = t.admit(ID_A, ADDR_2, Role::Peripheral, 185, now + 1);
        assert_eq!(adm, Admission::Accept);
        assert_eq!(
            displaced,
            Some(Displaced {
                identity: ID_A,
                addr: ADDR_1,
                role: Role::Central,
                silence_ms: 1,
            })
        );
        assert_eq!(t.link_count(), 1);
        assert_eq!(
            t.link_by_addr(&ADDR_2).map(|l| l.role),
            Some(Role::Peripheral)
        );
    }

    /// A link that has stopped answering ENTIRELY — no payload, no
    /// keepalive — does not hand itself to our dial either: the dial is
    /// refused at every age, and `expire` is what takes the link away
    /// (#382).
    ///
    /// Staleness is not a reason to displace. The age is measured on
    /// OUR receive path, so it says nothing about the peer's intent —
    /// a quiet link is equally a departed peer, a peer whose keepalives
    /// we are dropping, and a healthy phone — while the peer's own
    /// second connection does say something, which is why only that
    /// direction displaces. And it is not needed: the sweep below
    /// removes exactly the links this clause used to claim, on its own
    /// schedule, without replacing a peer's negotiated link with one we
    /// dialled.
    #[test]
    fn our_dial_is_refused_even_when_the_old_link_stopped_answering() {
        let mut t = table();
        t.admit(ID_A, ADDR_1, Role::Central, 185, 0);
        let frags = fragment_packet(b"data", 185);
        t.central_frame(ADDR_1, &frags[0], 1_000);

        for age in [
            1,
            LINK_TIMEOUT_MS - 1,
            LINK_TIMEOUT_MS,
            10 * LINK_TIMEOUT_MS,
        ] {
            let (adm, displaced) = t.admit(ID_A, ADDR_2, Role::Central, 185, 1_000 + age);
            assert_eq!(
                adm,
                Admission::RejectDuplicate {
                    old_silence_ms: age
                },
                "our own dial is refused at silence {age}, and the age is reported"
            );
            assert!(displaced.is_none(), "nothing displaced at silence {age}");
            assert_eq!(t.link_count(), 1);
        }

        // What does clear it: the expiry sweep, at the same bound, with
        // no dial involved at all.
        let expired = t.expire(1_000 + LINK_TIMEOUT_MS);
        assert_eq!(
            expired.links,
            vec![(ID_A, ADDR_1, Role::Central)],
            "the dead link is the expiry's job, and it does it"
        );
        assert_eq!(t.link_count(), 0);
    }

    /// The peer's own reconnect still displaces, at any age — the half
    /// of the rule 381fa5a0 got right, pinned against a change to the
    /// other half.
    #[test]
    fn the_peers_own_dial_still_displaces_however_fresh_the_old_link_is() {
        let mut t = table();
        t.admit(ID_A, ADDR_1, Role::Peripheral, 185, 0);
        let (adm, displaced) = t.admit(ID_A, ADDR_2, Role::Peripheral, 185, 1);
        assert_eq!(adm, Admission::Accept);
        assert_eq!(
            displaced.map(|d| (d.identity, d.addr, d.silence_ms)),
            Some((ID_A, ADDR_1, 1)),
            "one millisecond of silence, and the peer's own action decides"
        );
        assert_eq!(t.link_count(), 1);
    }

    #[test]
    fn max_links_bounds_both_roles_together() {
        let mut t = LinkTable::new(OWN, 2);
        assert_eq!(
            t.admit([1; 16], ADDR_1, Role::Central, 185, 0).0,
            Admission::Accept
        );
        assert_eq!(
            t.admit([2; 16], ADDR_2, Role::Peripheral, 185, 0).0,
            Admission::Accept
        );
        assert!(t.is_full());
        assert_eq!(
            t.admit([3; 16], ADDR_3, Role::Central, 185, 0).0,
            Admission::RejectFull
        );
    }

    /// The firmware's full-is-dark policy (`ADV_LOCK`, columba.rs at
    /// ead0bce), lnsd edition (#49 item 1): a full table must not
    /// advertise, and a freed slot puts the advertisement back on the
    /// air. Both roles count — `admit` refuses any newcomer once
    /// `max_links` is reached, so advertising while full would only
    /// invite connections that end in `RejectFull`.
    #[test]
    fn a_full_table_goes_dark_and_a_freed_slot_re_advertises() {
        let mut t = LinkTable::new(OWN, 2);
        assert!(t.should_advertise(), "empty table advertises");
        t.admit(ID_A, ADDR_1, Role::Peripheral, 185, 0);
        assert!(t.should_advertise(), "one free slot still advertises");
        t.admit(ID_B, ADDR_2, Role::Central, 185, 0);
        assert!(
            !t.should_advertise(),
            "full table is dark, both roles counted"
        );

        // A slot freed by disconnect re-advertises…
        t.remove_by_addr(&ADDR_2);
        assert!(t.should_advertise(), "a freed slot goes back on the air");

        // …and so does one freed by expiry.
        t.admit(ID_B, ADDR_2, Role::Central, 185, 0);
        assert!(!t.should_advertise());
        let expired = t.expire(LINK_TIMEOUT_MS);
        assert_eq!(expired.links.len(), 2);
        assert!(t.should_advertise(), "expiry frees the air too");
    }

    #[test]
    fn peripheral_handshake_opens_the_link_and_data_flows() {
        let mut t = table();
        // Pre-handshake data is ignored, keepalives tolerated.
        assert_eq!(
            t.peripheral_frame(ADDR_1, 185, &[0x01, 0, 0, 0, 1, 0xAA], 100),
            Inbound::NotHandshaked
        );
        assert_eq!(
            t.peripheral_frame(ADDR_1, 185, &[0x00], 200),
            Inbound::Keepalive
        );
        assert_eq!(t.link_count(), 0);

        // The 16-byte identity write opens the link.
        assert_eq!(
            t.peripheral_frame(ADDR_1, 185, &ID_B, 300),
            Inbound::HandshakeComplete {
                identity: ID_B,
                displaced: None
            }
        );
        assert_eq!(
            t.link_by_addr(&ADDR_1).map(|l| l.role),
            Some(Role::Peripheral)
        );

        // After the handshake a 16-byte frame is ordinary traffic, not a
        // repeated handshake (firmware reading; see peripheral_frame).
        let sixteen = [0x04; 16];
        assert_eq!(
            t.peripheral_frame(ADDR_1, 185, &sixteen, 400),
            Inbound::Error
        );

        // Multi-fragment reassembly across writes.
        let packet: Vec<u8> = (0..300).map(|i| (i % 251) as u8).collect();
        let frags = fragment_packet(&packet, 185);
        assert!(frags.len() > 1);
        let mut last = Inbound::NeedMore;
        for frag in &frags {
            last = t.peripheral_frame(ADDR_1, 185, frag, 500);
        }
        assert_eq!(last, Inbound::Packet(packet));
    }

    /// The #372 question, answered for lnsd: the peripheral side holds
    /// two centrals CONCURRENTLY. Both handshake, both links are live at
    /// once, their interleaved fragment streams reassemble on separate
    /// per-link state, the outbound fan-out serves both through the
    /// shared notify pipe at the smaller MTU, and losing one central
    /// leaves the other untouched. (The BlueZ layer above imposes no
    /// stricter limit: the advertisement stays registered while centrals
    /// are connected, writes arrive keyed by device address, and one
    /// notify reaches every subscriber — so this table IS the admission
    /// bound, `max_links` = 4 by default.)
    #[test]
    fn two_centrals_hold_concurrent_peripheral_links() {
        let mut t = table();
        assert_eq!(
            t.peripheral_frame(ADDR_1, 185, &ID_A, 100),
            Inbound::HandshakeComplete {
                identity: ID_A,
                displaced: None
            }
        );
        assert_eq!(
            t.peripheral_frame(ADDR_2, 23, &ID_B, 200),
            Inbound::HandshakeComplete {
                identity: ID_B,
                displaced: None
            },
            "the second central is admitted while the first is live"
        );
        assert_eq!(t.link_count(), 2);
        assert!(t.knows_identity(&ID_A) && t.knows_identity(&ID_B));

        // Interleaved multi-fragment traffic from both centrals: each
        // link reassembles on its own defragmenter, nothing crosses.
        let pkt_a: Vec<u8> = vec![0xA5; 300];
        let pkt_b: Vec<u8> = vec![0x5B; 300];
        let frags_a = fragment_packet(&pkt_a, 185);
        let frags_b = fragment_packet(&pkt_b, 23);
        assert_eq!(
            t.peripheral_frame(ADDR_1, 185, &frags_a[0], 300),
            Inbound::NeedMore
        );
        let mut last_b = Inbound::NeedMore;
        for frag in &frags_b {
            last_b = t.peripheral_frame(ADDR_2, 23, frag, 301);
        }
        assert_eq!(last_b, Inbound::Packet(pkt_b));
        assert_eq!(
            t.peripheral_frame(ADDR_1, 185, &frags_a[1], 302),
            Inbound::Packet(pkt_a),
            "B's whole stream in between did not touch A's reassembly"
        );
        assert!(t.take_abandon_reports().is_empty());

        // Outbound: one shared notify pipe, fragmented at the MINIMUM
        // peripheral MTU so the smaller subscriber gets whole fragments.
        let out: Vec<u8> = vec![0x77; 100];
        let plan = t.plan_tx(&out);
        assert_eq!(
            plan.notify_fragments.len(),
            100usize.div_ceil(payload_per_fragment(23))
        );
        assert!(plan.central.is_empty());

        // One central disconnecting is one loss; the other link stands.
        assert_eq!(t.remove_by_addr(&ADDR_2).map(|(id, ..)| id), Some(ID_B));
        assert!(!t.knows_identity(&ID_B));
        assert!(t.knows_identity(&ID_A), "the first central is untouched");
        assert_eq!(t.link_count(), 1);
    }

    #[test]
    fn a_rejected_peripheral_handshake_does_not_open_a_link() {
        let mut t = table();
        assert_eq!(
            t.peripheral_frame(ADDR_1, 185, &OWN, 100),
            Inbound::HandshakeRejected(Admission::RejectSelf)
        );
        assert_eq!(t.link_count(), 0);
    }

    #[test]
    fn tx_plan_fans_out_per_role_and_mtu() {
        let mut t = table();
        t.admit(ID_A, ADDR_1, Role::Central, 517, 0);
        t.admit(ID_B, ADDR_2, Role::Peripheral, 185, 0);
        t.admit([3; 16], ADDR_3, Role::Peripheral, 23, 0);

        let packet: Vec<u8> = vec![0x55; 400];
        let plan = t.plan_tx(&packet);

        // Central link fragments at its own MTU: 400 <= 509, one fragment.
        assert_eq!(plan.central.len(), 1);
        assert_eq!(plan.central[0].0, ADDR_1);
        assert_eq!(plan.central[0].1.len(), 1);

        // Notify pipe fragments at the MINIMUM peripheral MTU (23), so
        // the smallest subscriber still receives whole fragments.
        let expected = 400usize.div_ceil(payload_per_fragment(23));
        assert_eq!(plan.notify_fragments.len(), expected);

        // No peripheral links → no notify fragments.
        let mut t2 = table();
        t2.admit(ID_A, ADDR_1, Role::Central, 185, 0);
        assert!(t2.plan_tx(&packet).notify_fragments.is_empty());
    }

    /// The #376 hint, the case the field failure was made of: a routed
    /// packet for a central-role peer goes on that peer's pipe and on
    /// nothing else — no notify to the peripheral subscribers, which is
    /// where the board-to-board copy went.
    #[test]
    fn a_hinted_packet_takes_only_the_addressed_peers_link() {
        let mut t = table();
        t.admit(ID_A, ADDR_1, Role::Central, 517, 0);
        t.admit(ID_B, ADDR_2, Role::Peripheral, 185, 0);

        let packet: Vec<u8> = vec![0x55; 200];
        let plan = t.plan_tx_to(&packet, Some(&ID_A));
        assert_eq!(
            plan.route,
            TxRoute::Routed {
                peer: ID_A,
                role: Role::Central
            }
        );
        assert_eq!(plan.central.len(), 1);
        assert_eq!(plan.central[0].0, ADDR_1);
        assert!(
            plan.notify_fragments.is_empty(),
            "a packet routed at the central-role peer must not be \
             notified to the peripheral subscribers as well"
        );
    }

    /// Routing at a peripheral-role peer drops every CENTRAL link. The
    /// notify pipe is shared by construction (one characteristic, BlueZ
    /// fans it to every subscriber), and its fragments stay sized for
    /// the SMALLEST subscriber so none of them is truncated.
    #[test]
    fn a_hint_for_a_peripheral_peer_keeps_the_shared_notify_and_drops_the_centrals() {
        let mut t = table();
        t.admit(ID_A, ADDR_1, Role::Central, 517, 0);
        t.admit(ID_B, ADDR_2, Role::Peripheral, 185, 0);
        t.admit([3; 16], ADDR_3, Role::Peripheral, 23, 0);

        let packet: Vec<u8> = vec![0x55; 400];
        let plan = t.plan_tx_to(&packet, Some(&ID_B));
        assert_eq!(
            plan.route,
            TxRoute::Routed {
                peer: ID_B,
                role: Role::Peripheral
            }
        );
        assert!(
            plan.central.is_empty(),
            "the central link is not the addressee"
        );
        assert_eq!(
            plan.notify_fragments.len(),
            400usize.div_ceil(payload_per_fragment(23)),
            "fragmented for the smallest subscriber of the shared pipe"
        );
    }

    /// A peer holding a SECOND link — the rotated address reconnecting —
    /// is still one peer and gets the packet once. Here the table itself
    /// enforces that: `admit` refuses our own dial into a live link,
    /// whatever its age, and only once the old row is gone — expired,
    /// or displaced by the peer's own reconnect — does the new one
    /// exist, so one identity never owns two rows.
    /// (The firmware's registry does allow the two-slot displacement
    /// window; its own decision test is
    /// `leviculum_ble_tx::registry::a_peer_with_two_links_gets_the_packet_once`.)
    #[test]
    fn a_peer_that_reconnects_still_gets_the_packet_once() {
        let mut t = table();
        t.admit(ID_A, ADDR_1, Role::Central, 517, 0);
        assert_eq!(
            t.admit(ID_A, ADDR_2, Role::Central, 517, 0).0,
            Admission::RejectDuplicate { old_silence_ms: 0 },
            "a live link of the same identity refuses our own dial"
        );
        // The old row leaves the way a dead link leaves: the expiry
        // sweep. Only then is our dial a first link again.
        assert_eq!(
            t.expire(LINK_TIMEOUT_MS + 1).links,
            vec![(ID_A, ADDR_1, Role::Central)]
        );
        let (admission, displaced) = t.admit(ID_A, ADDR_2, Role::Central, 517, LINK_TIMEOUT_MS + 1);
        assert_eq!(admission, Admission::Accept);
        assert!(displaced.is_none(), "nothing left to displace");
        assert_eq!(t.live_links(), 1, "one identity, one row, always");

        let packet: Vec<u8> = vec![0x55; 100];
        let plan = t.plan_tx_to(&packet, Some(&ID_A));
        assert_eq!(plan.central.len(), 1, "one peer, one copy");
        assert_eq!(plan.central[0].0, ADDR_2, "and it is the newest link");
        assert!(plan.notify_fragments.is_empty());
    }

    /// The peer walked out between the core's routing decision and this
    /// plan: nothing is sent. Flooding the remaining links would spend
    /// their airtime on a packet they cannot deliver, and a neighbour
    /// that forwarded it would rebuild the duplicate #376 removes.
    #[test]
    fn a_hint_for_a_peer_with_no_link_sends_nothing() {
        let mut t = table();
        t.admit(ID_B, ADDR_2, Role::Peripheral, 185, 0);

        let packet: Vec<u8> = vec![0x55; 100];
        let plan = t.plan_tx_to(&packet, Some(&ID_A));
        assert_eq!(plan.route, TxRoute::NoLink { peer: ID_A });
        assert!(plan.central.is_empty());
        assert!(
            plan.notify_fragments.is_empty(),
            "the surviving peripheral link is not a route to the addressee"
        );
    }

    /// No hint is a broadcast and still reaches every live link — the
    /// announce case, which is what makes a peer discoverable at all.
    #[test]
    fn no_hint_still_floods_every_live_link() {
        let mut t = table();
        t.admit(ID_A, ADDR_1, Role::Central, 517, 0);
        t.admit(ID_B, ADDR_2, Role::Peripheral, 185, 0);

        let packet: Vec<u8> = vec![0x55; 100];
        let plan = t.plan_tx_to(&packet, None);
        assert_eq!(plan.route, TxRoute::Flood);
        assert_eq!(plan.central.len(), 1);
        assert!(!plan.notify_fragments.is_empty());
        assert_eq!(t.live_links(), 2);
    }

    #[test]
    fn keepalives_fire_per_cadence_and_shared_notify_rearms_all_peripherals() {
        let mut t = table();
        t.admit(ID_A, ADDR_1, Role::Central, 185, 0);
        t.admit(ID_B, ADDR_2, Role::Peripheral, 185, 0);
        t.admit([3; 16], ADDR_3, Role::Peripheral, 185, 5_000);

        // Before the interval: nothing due.
        let plan = t.keepalives_due(KEEPALIVE_INTERVAL_MS - 1);
        assert_eq!(plan, KeepalivePlan::default());

        // At the interval: central link due, and the first peripheral
        // link pulls the shared notify keepalive (rearming both).
        let plan = t.keepalives_due(KEEPALIVE_INTERVAL_MS);
        assert!(plan.notify);
        assert_eq!(plan.central, vec![ADDR_1]);

        // Immediately after: nothing due again, including the second
        // peripheral link that was rearmed by the shared send.
        let plan = t.keepalives_due(KEEPALIVE_INTERVAL_MS + 1);
        assert_eq!(plan, KeepalivePlan::default());
    }

    #[test]
    fn silent_links_and_stale_handshakes_expire() {
        let mut t = table();
        t.admit(ID_A, ADDR_1, Role::Central, 185, 0);
        t.peripheral_frame(ADDR_2, 185, &[0xFF; 20], 1_000); // pending, never identifies

        // A keepalive at 30s keeps the link alive past one interval.
        t.central_frame(ADDR_1, &[0x00], 30_000);

        let e = t.expire(31_000);
        assert_eq!(e.pending, vec![ADDR_2], "handshake timed out");
        assert!(e.links.is_empty(), "link was heard 1s ago");

        let e = t.expire(30_000 + LINK_TIMEOUT_MS);
        assert_eq!(e.links, vec![(ID_A, ADDR_1, Role::Central)]);
        assert_eq!(t.link_count(), 0);
    }

    /// The fallback clock's table-side busy input (#375 part 3): a
    /// pending handshake holds the clock — the attempt's outcome is
    /// about to reset it anyway — but a handshaked link does NOT. The
    /// eager spec: a linked board's clock runs, and only `knows_addr`
    /// keeps the linked address itself out of the window.
    #[test]
    fn pending_handshakes_are_busy_handshaked_links_are_not() {
        let mut t = table();
        assert!(!t.has_pending_handshakes());
        // An unidentified central: pending, no link — busy.
        t.peripheral_frame(ADDR_1, 185, &[0xFF; 20], 0);
        assert_eq!(t.link_count(), 0);
        assert!(t.has_pending_handshakes(), "a pending handshake is busy");
        let _ = t.expire(HANDSHAKE_TIMEOUT_MS);
        assert!(!t.has_pending_handshakes());

        // A handshaked link leaves the clock running.
        t.admit(ID_A, ADDR_2, Role::Central, 185, HANDSHAKE_TIMEOUT_MS);
        assert_eq!(t.link_count(), 1);
        assert!(
            !t.has_pending_handshakes(),
            "a live link must not suspend the clock"
        );
    }

    /// The #49 part 3 serialisation contract: one connection setup in
    /// flight per adapter, released on completion and on timeout alike
    /// (both end in the same [`DialQueue::release`], driven from the
    /// driver's `CentralIdentity`-accept and `CentralGone` events).
    #[test]
    fn the_dial_queue_admits_one_setup_and_releases_on_completion_and_timeout() {
        let mut q = DialQueue::new(7);
        assert!(q.is_idle());

        let a = q
            .enqueue(ADDR_1, ConnectDecision::InitiateLowerAddress, 1_000)
            .expect("first dial queues");
        assert_eq!(a.depth, 1);
        assert!((DIAL_JITTER_MIN_MS..=DIAL_JITTER_MAX_MS).contains(&a.wait_ms));
        assert!(!q.is_idle(), "a queued dial is busy");
        assert!(q.knows(&ADDR_1));

        // The jitter is served before the dial starts.
        assert_eq!(q.pop_ready(1_000 + a.wait_ms - 1), None);
        let popped = q.pop_ready(1_000 + a.wait_ms);
        assert_eq!(
            popped,
            Some((ADDR_1, ConnectDecision::InitiateLowerAddress))
        );
        assert!(q.knows(&ADDR_1), "in flight still blocks re-dialling");

        // A second dial queues behind the in-flight setup and cannot
        // start however long it waits.
        let b = q
            .enqueue(ADDR_2, ConnectDecision::InitiateFallback, 2_000)
            .expect("second dial queues");
        assert_eq!(b.depth, 1, "one dial waiting behind the setup");
        assert_eq!(q.next_deadline_ms(), None, "no start while in flight");
        assert_eq!(q.pop_ready(1_000_000), None);

        // Completion releases the slot; the timeout path is the same
        // release from the driver's CentralGone. The next dial serves
        // its OWN jitter from the release, not from its enqueue.
        q.release(&ADDR_1, 30_000);
        assert_eq!(q.next_deadline_ms(), Some(30_000 + b.wait_ms));
        assert_eq!(q.pop_ready(30_000 + b.wait_ms - 1), None);
        assert_eq!(
            q.pop_ready(30_000 + b.wait_ms),
            Some((ADDR_2, ConnectDecision::InitiateFallback))
        );
        q.release(&ADDR_2, 60_000);
        assert!(q.is_idle());

        // Stale releases (CentralGone after a completed session) are
        // no-ops.
        q.release(&ADDR_1, 61_000);
        assert!(q.is_idle());
    }

    /// The jitter contract: within `[DIAL_JITTER_MIN_MS,
    /// DIAL_JITTER_MAX_MS]` on every draw, deterministic per seed, and
    /// actually random across seeds.
    #[test]
    fn the_dial_jitter_stays_in_bounds_and_is_seeded() {
        let draws = |seed: u64| -> Vec<u64> {
            let mut q = DialQueue::new(seed);
            (0..32u64)
                .map(|i| {
                    let addr = [0xD0, 0, 0, 0, 0, i as u8];
                    let queued = q
                        .enqueue(addr, ConnectDecision::InitiateLowerAddress, i * 10_000)
                        .expect("fresh address queues");
                    let (popped, _) = q.pop_ready(i * 10_000 + queued.wait_ms).expect("ready");
                    q.release(&popped, i * 10_000 + queued.wait_ms);
                    queued.wait_ms
                })
                .collect()
        };
        let a = draws(7);
        assert!(a
            .iter()
            .all(|w| (DIAL_JITTER_MIN_MS..=DIAL_JITTER_MAX_MS).contains(w)));
        assert_eq!(a, draws(7), "same seed, same jitter sequence");
        assert_ne!(a, draws(8), "different seeds draw differently");
    }

    /// Duplicate elections do not stack: an address already queued or in
    /// flight is refused (and the driver logs nothing for it).
    #[test]
    fn a_known_address_does_not_enqueue_twice() {
        let mut q = DialQueue::new(7);
        let a = q
            .enqueue(ADDR_1, ConnectDecision::InitiateLowerAddress, 0)
            .expect("first");
        assert_eq!(
            q.enqueue(ADDR_1, ConnectDecision::InitiateFallback, 1),
            None
        );
        assert!(q.pop_ready(a.wait_ms).is_some());
        assert_eq!(
            q.enqueue(ADDR_1, ConnectDecision::InitiateFallback, 2),
            None,
            "in flight blocks too"
        );
        q.release(&ADDR_1, 10_000);
        assert!(
            q.enqueue(ADDR_1, ConnectDecision::InitiateFallback, 10_001)
                .is_some(),
            "after the setup ends the address may be dialled again"
        );
    }

    /// The driver's eligibility re-check refusing a popped dial releases
    /// the slot and the NEXT queued dial arms from that moment.
    #[test]
    fn a_refused_popped_dial_arms_the_next_one() {
        let mut q = DialQueue::new(7);
        let a = q
            .enqueue(ADDR_1, ConnectDecision::InitiateLowerAddress, 0)
            .expect("first");
        let b = q
            .enqueue(ADDR_2, ConnectDecision::InitiateLowerAddress, 0)
            .expect("second");
        assert_eq!(b.depth, 2);
        let t_pop = a.wait_ms;
        assert!(q.pop_ready(t_pop).is_some());
        // The driver's re-check says no (table filled meanwhile): the
        // release lets ADDR_2 serve its jitter from now.
        q.release(&ADDR_1, t_pop);
        assert_eq!(q.next_deadline_ms(), Some(t_pop + b.wait_ms));
        assert_eq!(
            q.pop_ready(t_pop + b.wait_ms).map(|(addr, _)| addr),
            Some(ADDR_2)
        );
    }

    #[test]
    fn peripheral_write_updates_the_link_mtu() {
        let mut t = table();
        t.peripheral_frame(ADDR_1, 23, &ID_B, 0);
        assert_eq!(t.link_by_addr(&ADDR_1).map(|l| l.mtu), Some(23));
        // MTU renegotiation surfaces on the next write.
        t.peripheral_frame(ADDR_1, 185, &[0x00], 100);
        assert_eq!(t.link_by_addr(&ADDR_1).map(|l| l.mtu), Some(185));
    }
}
