//! Discovered-interface registry records: the persisted form of a validated
//! discovery announce (sub-task c of #32).
//!
//! Where [`DiscoveredInterface`] is the transient event a received-and-validated
//! announce produces, a [`DiscoveredInterfaceRecord`] is its persisted form: the
//! announce fields plus the bookkeeping Python keeps per record (`discovered` /
//! `last_heard` / `heard_count` timestamps, the `hops` distance, and the
//! ready-to-paste `config_entry`). The msgpack layout is a byte-compatible port
//! of what Python `RNS.Discovery.InterfaceDiscovery` writes under
//! `storage/discovery/interfaces/<discovery_hash>`, so the two stacks share a
//! storage directory drop-in (`rnstatus -d` reads our files and vice versa).
//!
//! This module owns only the pure record shape, its `config_entry` derivation,
//! and its msgpack codec. The filesystem layout (one file per record, keyed by
//! the hex `discovery_hash`) and the read/modify/write dedup lifecycle live in
//! the std layer, which has the storage handle.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::stamp::STAMP_SIZE;
use super::DiscoveredInterface;
use crate::resource::msgpack;

/// Age past which a record is considered `unknown` (Python
/// `InterfaceDiscovery.THRESHOLD_UNKNOWN`, 24 h).
pub const THRESHOLD_UNKNOWN: f64 = 24.0 * 60.0 * 60.0;
/// Age past which a record is considered `stale` (Python
/// `InterfaceDiscovery.THRESHOLD_STALE`, 3 days).
pub const THRESHOLD_STALE: f64 = 3.0 * 24.0 * 60.0 * 60.0;
/// Age past which a record is dropped on load (Python
/// `InterfaceDiscovery.THRESHOLD_REMOVE`, 7 days).
pub const THRESHOLD_REMOVE: f64 = 7.0 * 24.0 * 60.0 * 60.0;

/// Interface types a persisted record may carry (Python
/// `InterfaceDiscovery.DISCOVERABLE_TYPES`). A record whose `type` is not in
/// this set is dropped on load. Note this excludes `TCPClientInterface`, which
/// the announce path recognises but the registry never stores.
pub const DISCOVERABLE_TYPES: [&str; 6] = [
    "BackboneInterface",
    "TCPServerInterface",
    "I2PInterface",
    "RNodeInterface",
    "WeaveInterface",
    "KISSInterface",
];

/// Liveness status derived from a record's `last_heard` age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryStatus {
    /// Heard within [`THRESHOLD_UNKNOWN`].
    Available,
    /// Between [`THRESHOLD_UNKNOWN`] and [`THRESHOLD_STALE`].
    Unknown,
    /// Older than [`THRESHOLD_STALE`] (but not yet removed).
    Stale,
}

impl DiscoveryStatus {
    /// The lowercase status string Python stores in `info["status"]`.
    pub fn as_str(self) -> &'static str {
        match self {
            DiscoveryStatus::Available => "available",
            DiscoveryStatus::Unknown => "unknown",
            DiscoveryStatus::Stale => "stale",
        }
    }

    /// The numeric `status_code` Python sorts on
    /// (`InterfaceDiscovery.STATUS_CODE_MAP`).
    pub fn code(self) -> u32 {
        match self {
            DiscoveryStatus::Available => 1000,
            DiscoveryStatus::Unknown => 100,
            DiscoveryStatus::Stale => 0,
        }
    }
}

/// A persisted discovered-interface record.
///
/// Mirrors the string-keyed msgpack dict Python persists per discovered
/// interface. `transport_id` / `network_id` are stored as lowercase hex strings
/// (Python `RNS.hexrep(..., delimit=False)`); `stamp` and `discovery_hash` are
/// raw bytes; `discovered` / `last_heard` / `received` are Unix-epoch seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredInterfaceRecord {
    /// Interface type name (Python `type`).
    pub interface_type: String,
    /// Whether the announcing node has transport enabled.
    pub transport: bool,
    /// Sanitised interface name.
    pub name: String,
    /// Wall-clock (Unix seconds) the announce was received.
    pub received: f64,
    /// The 32-byte proof-of-work stamp.
    pub stamp: [u8; STAMP_SIZE],
    /// Realised stamp value (leading zero bits).
    pub value: u32,
    /// Announcing node's transport identity, lowercase hex.
    pub transport_id: String,
    /// Announcing network identity, lowercase hex.
    pub network_id: String,
    /// Distance in hops at reception time (Python `Transport.hops_to`).
    pub hops: u32,
    /// Latitude, if published.
    pub latitude: Option<f64>,
    /// Longitude, if published.
    pub longitude: Option<f64>,
    /// Height, if published.
    pub height: Option<f64>,
    /// IFAC network name, if published.
    pub ifac_netname: Option<String>,
    /// IFAC network key, if published.
    pub ifac_netkey: Option<String>,
    /// Reachable host/IP (TCPServer/Backbone/I2P).
    pub reachable_on: Option<String>,
    /// Bind port (TCPServer/Backbone).
    pub port: Option<u64>,
    /// Radio frequency in Hz (RNode).
    pub frequency: Option<u64>,
    /// Radio bandwidth in Hz (RNode).
    pub bandwidth: Option<u64>,
    /// Spreading factor (RNode; Python `sf`).
    pub sf: Option<u64>,
    /// Coding rate (RNode; Python `cr`).
    pub cr: Option<u64>,
    /// Ready-to-paste config entry (`rnstatus -D`), for types that support it.
    pub config_entry: Option<String>,
    /// Stable per-endpoint hash, `full_hash(hex(transport_id) + name)`.
    pub discovery_hash: [u8; STAMP_SIZE],
    /// First-seen timestamp (preserved across re-announces).
    pub discovered: f64,
    /// Most-recent-heard timestamp.
    pub last_heard: f64,
    /// Number of times re-heard after the first (Python `heard_count`).
    pub heard_count: u64,
}

impl DiscoveredInterfaceRecord {
    /// Build a record from a freshly validated [`DiscoveredInterface`].
    ///
    /// `hops` is the reception-time distance (the caller supplies it from the
    /// transport path table). `received` is the reception wall-clock; on the
    /// first sighting `discovered` == `last_heard` == `received` and
    /// `heard_count` == 0, matching Python `interface_discovered`.
    pub fn from_discovered(
        di: &DiscoveredInterface,
        hops: u32,
        received: f64,
        discovered: f64,
        last_heard: f64,
        heard_count: u64,
    ) -> Self {
        let transport_id = hex_lower(&di.transport_id);
        let network_id = hex_lower(&di.network_id);
        let config_entry = build_config_entry(di, &transport_id);

        DiscoveredInterfaceRecord {
            interface_type: di.interface_type.clone(),
            transport: di.transport,
            name: di.name.clone(),
            received,
            stamp: di.stamp,
            value: di.value,
            transport_id,
            network_id,
            hops,
            latitude: di.latitude,
            longitude: di.longitude,
            height: di.height,
            ifac_netname: di.ifac_netname.clone(),
            ifac_netkey: di.ifac_netkey.clone(),
            reachable_on: di.reachable_on.clone(),
            port: di.port,
            frequency: di.frequency,
            bandwidth: di.bandwidth,
            sf: di.spreadingfactor,
            cr: di.codingrate,
            config_entry,
            discovery_hash: di.discovery_hash,
            discovered,
            last_heard,
            heard_count,
        }
    }

    /// Lowercase-hex `discovery_hash` — the on-disk filename Python uses
    /// (`RNS.hexrep(discovery_hash, delimit=False)`).
    pub fn discovery_hash_hex(&self) -> String {
        hex_lower(&self.discovery_hash)
    }

    /// Liveness status for a given wall-clock `now` (Unix seconds).
    pub fn status(&self, now: f64) -> DiscoveryStatus {
        let heard_delta = now - self.last_heard;
        if heard_delta > THRESHOLD_STALE {
            DiscoveryStatus::Stale
        } else if heard_delta > THRESHOLD_UNKNOWN {
            DiscoveryStatus::Unknown
        } else {
            DiscoveryStatus::Available
        }
    }

    /// Whether this record should be dropped on load: too old, or a type the
    /// registry does not store (Python `list_discovered_interfaces` removal).
    pub fn should_remove(&self, now: f64) -> bool {
        (now - self.last_heard) > THRESHOLD_REMOVE
            || !DISCOVERABLE_TYPES.contains(&self.interface_type.as_str())
    }

    /// Encode this record as Python-compatible string-keyed msgpack.
    pub fn encode_msgpack(&self) -> Vec<u8> {
        // Build the body first so the exact key count is known for the header.
        let mut body = Vec::new();
        let mut count: u32 = 0;

        macro_rules! kv {
            ($key:expr, $write:expr) => {{
                msgpack::write_str(&mut body, $key);
                $write;
                count += 1;
            }};
        }

        kv!("type", msgpack::write_str(&mut body, &self.interface_type));
        kv!("transport", msgpack::write_bool(&mut body, self.transport));
        kv!("name", msgpack::write_str(&mut body, &self.name));
        kv!("received", msgpack::write_float64(&mut body, self.received));
        kv!("stamp", msgpack::write_bin(&mut body, &self.stamp));
        kv!("value", msgpack::write_uint(&mut body, self.value as u64));
        kv!(
            "transport_id",
            msgpack::write_str(&mut body, &self.transport_id)
        );
        kv!(
            "network_id",
            msgpack::write_str(&mut body, &self.network_id)
        );
        kv!("hops", msgpack::write_uint(&mut body, self.hops as u64));
        kv!("latitude", write_float_or_nil(&mut body, self.latitude));
        kv!("longitude", write_float_or_nil(&mut body, self.longitude));
        kv!("height", write_float_or_nil(&mut body, self.height));

        if let Some(v) = &self.ifac_netname {
            kv!("ifac_netname", msgpack::write_str(&mut body, v));
        }
        if let Some(v) = &self.ifac_netkey {
            kv!("ifac_netkey", msgpack::write_str(&mut body, v));
        }
        if let Some(v) = &self.reachable_on {
            kv!("reachable_on", msgpack::write_str(&mut body, v));
        }
        if let Some(v) = self.port {
            kv!("port", msgpack::write_uint(&mut body, v));
        }
        if let Some(v) = self.frequency {
            kv!("frequency", msgpack::write_uint(&mut body, v));
        }
        if let Some(v) = self.bandwidth {
            kv!("bandwidth", msgpack::write_uint(&mut body, v));
        }
        if let Some(v) = self.sf {
            kv!("sf", msgpack::write_uint(&mut body, v));
        }
        if let Some(v) = self.cr {
            kv!("cr", msgpack::write_uint(&mut body, v));
        }
        if let Some(v) = &self.config_entry {
            kv!("config_entry", msgpack::write_str(&mut body, v));
        }

        kv!(
            "discovery_hash",
            msgpack::write_bin(&mut body, &self.discovery_hash)
        );
        kv!(
            "discovered",
            msgpack::write_float64(&mut body, self.discovered)
        );
        kv!(
            "last_heard",
            msgpack::write_float64(&mut body, self.last_heard)
        );
        kv!(
            "heard_count",
            msgpack::write_uint(&mut body, self.heard_count)
        );

        let mut out = Vec::with_capacity(3 + body.len());
        write_map_header(&mut out, count);
        out.extend_from_slice(&body);
        out
    }

    /// Decode a persisted record from string-keyed msgpack (Python or ours).
    ///
    /// Unknown keys (e.g. Weave/KISS `modulation` / `channel`, which the
    /// announce path does not yet surface) are skipped for forward
    /// compatibility. Returns `None` on malformed input or a missing required
    /// base field.
    pub fn decode_msgpack(data: &[u8]) -> Option<Self> {
        let mut pos = 0usize;
        let count = read_map_len(data, &mut pos)?;

        let mut interface_type: Option<String> = None;
        let mut transport: Option<bool> = None;
        let mut name: Option<String> = None;
        let mut received: Option<f64> = None;
        let mut stamp: Option<[u8; STAMP_SIZE]> = None;
        let mut value: Option<u32> = None;
        let mut transport_id: Option<String> = None;
        let mut network_id: Option<String> = None;
        let mut hops: Option<u32> = None;
        let mut latitude: Option<f64> = None;
        let mut longitude: Option<f64> = None;
        let mut height: Option<f64> = None;
        let mut ifac_netname: Option<String> = None;
        let mut ifac_netkey: Option<String> = None;
        let mut reachable_on: Option<String> = None;
        let mut port: Option<u64> = None;
        let mut frequency: Option<u64> = None;
        let mut bandwidth: Option<u64> = None;
        let mut sf: Option<u64> = None;
        let mut cr: Option<u64> = None;
        let mut config_entry: Option<String> = None;
        let mut discovery_hash: Option<[u8; STAMP_SIZE]> = None;
        let mut discovered: Option<f64> = None;
        let mut last_heard: Option<f64> = None;
        let mut heard_count: Option<u64> = None;

        for _ in 0..count {
            let key = read_str(data, &mut pos)?;
            match key.as_str() {
                "type" => interface_type = Some(read_str(data, &mut pos)?),
                "transport" => transport = Some(msgpack::read_bool(data, &mut pos)?),
                "name" => name = Some(read_str(data, &mut pos)?),
                "received" => received = Some(msgpack::read_float64(data, &mut pos)?),
                "stamp" => stamp = Some(read_bin_array(data, &mut pos)?),
                "value" => value = Some(msgpack::read_msgpack_uint(data, &mut pos)? as u32),
                "transport_id" => transport_id = Some(read_str(data, &mut pos)?),
                "network_id" => network_id = Some(read_str(data, &mut pos)?),
                "hops" => hops = Some(msgpack::read_msgpack_uint(data, &mut pos)? as u32),
                "latitude" => latitude = read_float_or_nil(data, &mut pos)?,
                "longitude" => longitude = read_float_or_nil(data, &mut pos)?,
                "height" => height = read_float_or_nil(data, &mut pos)?,
                "ifac_netname" => ifac_netname = Some(read_str(data, &mut pos)?),
                "ifac_netkey" => ifac_netkey = Some(read_str(data, &mut pos)?),
                "reachable_on" => reachable_on = Some(read_str(data, &mut pos)?),
                "port" => port = Some(msgpack::read_msgpack_uint(data, &mut pos)?),
                "frequency" => frequency = Some(msgpack::read_msgpack_uint(data, &mut pos)?),
                "bandwidth" => bandwidth = Some(msgpack::read_msgpack_uint(data, &mut pos)?),
                "sf" => sf = Some(msgpack::read_msgpack_uint(data, &mut pos)?),
                "cr" => cr = Some(msgpack::read_msgpack_uint(data, &mut pos)?),
                "config_entry" => config_entry = Some(read_str(data, &mut pos)?),
                "discovery_hash" => discovery_hash = Some(read_bin_array(data, &mut pos)?),
                "discovered" => discovered = Some(msgpack::read_float64(data, &mut pos)?),
                "last_heard" => last_heard = Some(msgpack::read_float64(data, &mut pos)?),
                "heard_count" => heard_count = Some(msgpack::read_msgpack_uint(data, &mut pos)?),
                _ => msgpack::skip_msgpack_value(data, &mut pos)?,
            }
        }

        // `received` predates `discovered`/`last_heard` in Python's write, but a
        // freshly persisted record always has all three. Fall back gracefully so
        // a partially written record still loads with sane timestamps.
        let received = received?;
        Some(DiscoveredInterfaceRecord {
            interface_type: interface_type?,
            transport: transport?,
            name: name?,
            received,
            stamp: stamp?,
            value: value?,
            transport_id: transport_id?,
            network_id: network_id?,
            hops: hops.unwrap_or(0),
            latitude,
            longitude,
            height,
            ifac_netname,
            ifac_netkey,
            reachable_on,
            port,
            frequency,
            bandwidth,
            sf,
            cr,
            config_entry,
            discovery_hash: discovery_hash?,
            discovered: discovered.unwrap_or(received),
            last_heard: last_heard.unwrap_or(received),
            heard_count: heard_count.unwrap_or(0),
        })
    }
}

/// Build the `config_entry` string for a discovered interface, matching Python
/// `InterfaceAnnounceHandler.received_announce`. Returns `None` for types that
/// have no config-entry form (or that are missing a required field).
fn build_config_entry(di: &DiscoveredInterface, transport_id_hex: &str) -> Option<String> {
    let name = &di.name;
    let netname_str = di
        .ifac_netname
        .as_deref()
        .map(|v| format!("\n  network_name = {v}"))
        .unwrap_or_default();
    let netkey_str = di
        .ifac_netkey
        .as_deref()
        .map(|v| format!("\n  passphrase = {v}"))
        .unwrap_or_default();
    let identity_str = format!("\n  transport_identity = {transport_id_hex}");

    match di.interface_type.as_str() {
        // On non-Windows hosts Python advertises Backbone/TCPServer as a
        // BackboneClientInterface (remote = <host>), so both stored types render
        // the same config entry.
        "BackboneInterface" | "TCPServerInterface" => {
            let remote = di.reachable_on.as_deref()?;
            let port = di.port?;
            Some(format!(
                "[[{name}]]\n  type = BackboneInterface\n  enabled = yes\n  \
                 remote = {remote}\n  target_port = {port}{identity_str}{netname_str}{netkey_str}"
            ))
        }
        "I2PInterface" => {
            let remote = di.reachable_on.as_deref()?;
            Some(format!(
                "[[{name}]]\n  type = I2PInterface\n  enabled = yes\n  \
                 peers = {remote}{identity_str}{netname_str}{netkey_str}"
            ))
        }
        "RNodeInterface" => {
            let frequency = di.frequency?;
            let bandwidth = di.bandwidth?;
            let sf = di.spreadingfactor?;
            let cr = di.codingrate?;
            // Python omits transport_identity from the RNode config entry.
            Some(format!(
                "[[{name}]]\n  type = RNodeInterface\n  enabled = yes\n  port = \n  \
                 frequency = {frequency}\n  bandwidth = {bandwidth}\n  \
                 spreadingfactor = {sf}\n  codingrate = {cr}\n  \
                 txpower = {netname_str}{netkey_str}"
            ))
        }
        // Weave/KISS need modulation/channel fields the announce path does not
        // yet surface; TCPClient has no config-entry form. No entry emitted.
        _ => None,
    }
}

/// Lowercase hex of a byte slice (`RNS.hexrep(..., delimit=False)`).
fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Write a msgpack map header (fixmap for < 16 entries, else map16/map32),
/// matching `umsgpack._pack_map`.
fn write_map_header(buf: &mut Vec<u8>, count: u32) {
    if count < 16 {
        buf.push(0x80 | (count as u8));
    } else if count < 65536 {
        buf.push(0xde);
        buf.extend_from_slice(&(count as u16).to_be_bytes());
    } else {
        buf.push(0xdf);
        buf.extend_from_slice(&count.to_be_bytes());
    }
}

/// Read a msgpack map length (fixmap or map16/map32).
fn read_map_len(data: &[u8], pos: &mut usize) -> Option<usize> {
    let tag = *data.get(*pos)?;
    if tag & 0xf0 == 0x80 {
        *pos += 1;
        Some((tag & 0x0f) as usize)
    } else if tag == 0xde {
        *pos += 1;
        Some(msgpack::read_be_u16(data, pos)? as usize)
    } else if tag == 0xdf {
        *pos += 1;
        Some(msgpack::read_be_u32(data, pos)? as usize)
    } else {
        None
    }
}

/// Read a msgpack string into an owned `String`.
fn read_str(data: &[u8], pos: &mut usize) -> Option<String> {
    let bytes = msgpack::read_msgpack_str(data, pos)?;
    Some(core::str::from_utf8(bytes).ok()?.into())
}

/// Read a msgpack binary into a fixed-size array (stamp / discovery_hash).
fn read_bin_array(data: &[u8], pos: &mut usize) -> Option<[u8; STAMP_SIZE]> {
    let bytes = msgpack::read_msgpack_bin(data, pos)?;
    bytes.try_into().ok()
}

/// Write a float64 or msgpack nil.
fn write_float_or_nil(buf: &mut Vec<u8>, value: Option<f64>) {
    match value {
        Some(v) => msgpack::write_float64(buf, v),
        None => msgpack::write_nil(buf),
    }
}

/// Read a float64 or msgpack nil. Outer `Option` is the parse result; inner is
/// present-vs-nil.
fn read_float_or_nil(data: &[u8], pos: &mut usize) -> Option<Option<f64>> {
    if data.get(*pos) == Some(&0xc0) {
        *pos += 1;
        return Some(None);
    }
    Some(Some(msgpack::read_float64(data, pos)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn rnode_di() -> DiscoveredInterface {
        DiscoveredInterface {
            interface_type: "RNodeInterface".to_string(),
            transport: true,
            name: "Node A".to_string(),
            transport_id: [0xAB; 16],
            network_id: [0xCD; 16],
            value: 15,
            stamp: [0x11; STAMP_SIZE],
            latitude: Some(52.5),
            longitude: Some(13.4),
            height: None,
            reachable_on: None,
            port: None,
            frequency: Some(867_200_000),
            bandwidth: Some(125_000),
            spreadingfactor: Some(8),
            codingrate: Some(5),
            ifac_netname: None,
            ifac_netkey: None,
            discovery_hash: [0x22; STAMP_SIZE],
        }
    }

    fn backbone_di() -> DiscoveredInterface {
        DiscoveredInterface {
            interface_type: "BackboneInterface".to_string(),
            transport: true,
            name: "Hub".to_string(),
            transport_id: [0xAB; 16],
            network_id: [0xAB; 16],
            value: 20,
            stamp: [0x33; STAMP_SIZE],
            latitude: None,
            longitude: None,
            height: None,
            reachable_on: Some("10.0.0.5".to_string()),
            port: Some(4965),
            frequency: None,
            bandwidth: None,
            spreadingfactor: None,
            codingrate: None,
            ifac_netname: None,
            ifac_netkey: None,
            discovery_hash: [0x44; STAMP_SIZE],
        }
    }

    #[test]
    fn from_discovered_populates_fields_and_timestamps() {
        let di = rnode_di();
        let rec = DiscoveredInterfaceRecord::from_discovered(&di, 2, 1000.0, 1000.0, 1000.0, 0);

        assert_eq!(rec.interface_type, "RNodeInterface");
        assert_eq!(rec.name, "Node A");
        assert_eq!(rec.value, 15);
        assert_eq!(rec.hops, 2);
        assert_eq!(rec.transport_id, "abababababababababababababababab");
        assert_eq!(rec.network_id, "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd");
        assert_eq!(rec.frequency, Some(867_200_000));
        assert_eq!(rec.sf, Some(8));
        assert_eq!(rec.cr, Some(5));
        assert_eq!(rec.received, 1000.0);
        assert_eq!(rec.discovered, 1000.0);
        assert_eq!(rec.last_heard, 1000.0);
        assert_eq!(rec.heard_count, 0);
        assert_eq!(rec.discovery_hash_hex(), hex_lower(&[0x22; STAMP_SIZE]));
        assert!(rec.config_entry.is_some());
    }

    #[test]
    fn config_entry_matches_python_rnode() {
        let di = rnode_di();
        let rec = DiscoveredInterfaceRecord::from_discovered(&di, 1, 0.0, 0.0, 0.0, 0);
        let expected = "[[Node A]]\n  type = RNodeInterface\n  enabled = yes\n  port = \n  \
                        frequency = 867200000\n  bandwidth = 125000\n  spreadingfactor = 8\n  \
                        codingrate = 5\n  txpower = ";
        assert_eq!(rec.config_entry.as_deref(), Some(expected));
    }

    #[test]
    fn config_entry_matches_python_backbone() {
        let di = backbone_di();
        let rec = DiscoveredInterfaceRecord::from_discovered(&di, 1, 0.0, 0.0, 0.0, 0);
        let expected = "[[Hub]]\n  type = BackboneInterface\n  enabled = yes\n  \
                        remote = 10.0.0.5\n  target_port = 4965\n  \
                        transport_identity = abababababababababababababababab";
        assert_eq!(rec.config_entry.as_deref(), Some(expected));
    }

    #[test]
    fn config_entry_includes_ifac_when_present() {
        let mut di = backbone_di();
        di.ifac_netname = Some("mynet".to_string());
        di.ifac_netkey = Some("secret".to_string());
        let rec = DiscoveredInterfaceRecord::from_discovered(&di, 1, 0.0, 0.0, 0.0, 0);
        let ce = rec.config_entry.unwrap();
        assert!(ce.ends_with("\n  network_name = mynet\n  passphrase = secret"));
    }

    #[test]
    fn msgpack_round_trips() {
        for di in [rnode_di(), backbone_di()] {
            let rec = DiscoveredInterfaceRecord::from_discovered(&di, 3, 1700.0, 1600.0, 1700.0, 4);
            let bytes = rec.encode_msgpack();
            let decoded =
                DiscoveredInterfaceRecord::decode_msgpack(&bytes).expect("record must decode back");
            assert_eq!(decoded, rec);
        }
    }

    #[test]
    fn decode_skips_unknown_keys() {
        // A record with an extra unrecognised key (a Weave `modulation` string)
        // must still decode: unknown keys are skipped for forward compat.
        let di = rnode_di();
        let rec = DiscoveredInterfaceRecord::from_discovered(&di, 1, 1.0, 1.0, 1.0, 0);
        let mut bytes = rec.encode_msgpack();
        // Bump the map header entry count (fixmap or map16) and append one k/v.
        // Records have > 15 keys, so the header is map16 (0xde + u16).
        assert_eq!(bytes[0], 0xde);
        let count = u16::from_be_bytes([bytes[1], bytes[2]]);
        let new_count = (count + 1).to_be_bytes();
        bytes[1] = new_count[0];
        bytes[2] = new_count[1];
        msgpack::write_str(&mut bytes, "modulation");
        msgpack::write_str(&mut bytes, "LoRa");
        let decoded = DiscoveredInterfaceRecord::decode_msgpack(&bytes).expect("decodes");
        assert_eq!(decoded, rec);
    }

    #[test]
    fn status_thresholds() {
        let di = rnode_di();
        let rec = DiscoveredInterfaceRecord::from_discovered(&di, 1, 0.0, 0.0, 0.0, 0);
        assert_eq!(rec.status(10.0), DiscoveryStatus::Available);
        assert_eq!(
            rec.status(THRESHOLD_UNKNOWN + 1.0),
            DiscoveryStatus::Unknown
        );
        assert_eq!(rec.status(THRESHOLD_STALE + 1.0), DiscoveryStatus::Stale);
        assert!(!rec.should_remove(THRESHOLD_STALE + 1.0));
        assert!(rec.should_remove(THRESHOLD_REMOVE + 1.0));
        assert_eq!(DiscoveryStatus::Available.code(), 1000);
    }
}
