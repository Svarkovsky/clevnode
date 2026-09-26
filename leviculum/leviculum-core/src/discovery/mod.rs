//! On-network interface discovery: announce `app_data` wire format + PoW stamp.
//!
//! A node advertises its discoverable interfaces by announcing on the
//! `rnstransport.discovery.interface` destination. The announce `app_data`
//! carries a proof-of-work-stamped, integer-keyed msgpack description of one
//! interface. Peers validate the stamp and surface the description as a
//! [`DiscoveredInterface`] for higher layers to act on.
//!
//! This module is a byte-for-byte port of Python `RNS.Discovery`
//! (`reference/Reticulum/RNS/Discovery.py`) and the stamp scheme in
//! `LXMF.LXStamper`. It covers sub-task (a) of interface auto-discovery:
//! encode, decode, validate, and surface. It performs NO auto-connect and keeps
//! NO persistent registry; those are later sub-tasks that consume the
//! [`DiscoveredInterface`] surfaced here.
//!
//! # Wire format
//!
//! Plaintext: `app_data = flags(1) + msgpack(info) + stamp(32)`
//!
//! Encrypted (private discovery network): the flags byte is left unencrypted
//! and the `msgpack(info) + stamp(32)` tail is encrypted with a shared network
//! identity, giving `app_data = flags(1) + network_identity.encrypt(msgpack(info) + stamp(32))`.
//! The `FLAG_ENCRYPTED` bit signals this. The encryption primitive is the same
//! `Identity::encrypt` used everywhere else (X25519 ephemeral + HKDF + token),
//! matching Python `Identity.encrypt` byte-for-byte, so only holders of the
//! network identity's private key can decode discoverable neighbours.
//!
//! `info` is an integer-keyed msgpack map (see the `key` constants). The stamp
//! covers `full_hash(msgpack(info))`. The interface supplies only its
//! [`InterfaceDescriptor`]; the protocol ordering and stamping live here, so
//! the carrier medium stays isolated from discovery mechanics.

mod registry;
mod stamp;

pub use registry::{
    DiscoveredInterfaceRecord, DiscoveryStatus, DISCOVERABLE_TYPES, THRESHOLD_REMOVE,
    THRESHOLD_STALE, THRESHOLD_UNKNOWN,
};
pub use stamp::{
    generate_stamp, stamp_valid, stamp_value, stamp_workblock, StampError, DEFAULT_STAMP_VALUE,
    MIN_REQUIRED_STAMP_VALUE, STAMP_SIZE, WORKBLOCK_EXPAND_ROUNDS,
};

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rand_core::CryptoRngCore;

use crate::constants::TRUNCATED_HASHBYTES;
use crate::crypto::full_hash;
use crate::identity::Identity;
use crate::resource::msgpack;

/// Application name for the discovery destination.
pub const APP_NAME: &str = "rnstransport";

/// Aspects of the discovery destination (`rnstransport.discovery.interface`).
pub const DISCOVERY_ASPECTS: [&str; 2] = ["discovery", "interface"];

/// The announce-handler aspect filter for discovery announces.
pub const DISCOVERY_ASPECT_FILTER: &str = "rnstransport.discovery.interface";

// Announce flag bits (Discovery.py InterfaceAnnounceHandler).
const FLAG_SIGNED: u8 = 0b0000_0001;
const FLAG_ENCRYPTED: u8 = 0b0000_0010;

// Integer msgpack keys for the `info` map (Discovery.py:12-28).
const KEY_INTERFACE_TYPE: u64 = 0x00;
const KEY_TRANSPORT: u64 = 0x01;
const KEY_REACHABLE_ON: u64 = 0x02;
const KEY_LATITUDE: u64 = 0x03;
const KEY_LONGITUDE: u64 = 0x04;
const KEY_HEIGHT: u64 = 0x05;
const KEY_PORT: u64 = 0x06;
const KEY_IFAC_NETNAME: u64 = 0x07;
const KEY_IFAC_NETKEY: u64 = 0x08;
const KEY_FREQUENCY: u64 = 0x09;
const KEY_BANDWIDTH: u64 = 0x0A;
const KEY_SPREADINGFACTOR: u64 = 0x0B;
const KEY_CODINGRATE: u64 = 0x0C;
const KEY_MODULATION: u64 = 0x0D;
const KEY_CHANNEL: u64 = 0x0E;
/// Operator LXMF address, added by RNS 1.5.0 (`Discovery.py` `OP_ADDR`).
/// Optional on the wire and not surfaced by us yet; listed here so the field is
/// accounted for rather than merely swept up by the unknown-key arm.
const KEY_OP_ADDR: u64 = 0xF0;
const KEY_TRANSPORT_ID: u64 = 0xFE;
const KEY_NAME: u64 = 0xFF;

/// Interface types that participate in discovery (Discovery.py
/// `DISCOVERABLE_INTERFACE_TYPES`).
pub const DISCOVERABLE_INTERFACE_TYPES: [&str; 7] = [
    "BackboneInterface",
    "TCPServerInterface",
    "TCPClientInterface",
    "RNodeInterface",
    "WeaveInterface",
    "I2PInterface",
    "KISSInterface",
];

/// Per-interface discovery descriptor, supplied BY the interface.
///
/// Per the interface-isolation rule, only the interface knows its own carrier
/// details (reachable address, radio parameters). Discovery here owns the
/// protocol ordering and the stamp; the interface owns the values. The set of
/// fields emitted is driven by [`interface_type`](Self::interface_type),
/// matching Python `get_interface_announce_data`.
///
/// For sub-task (a) the descriptor is intentionally minimal: base fields plus
/// the address/radio fields needed by `TCPServerInterface`/`BackboneInterface`
/// and `RNodeInterface`. Weave/KISS/I2P specifics extend this in (b).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InterfaceDescriptor {
    /// Interface type name, e.g. `"RNodeInterface"`.
    pub interface_type: String,
    /// Human-readable interface name (`None` encodes msgpack nil).
    pub name: Option<String>,
    /// Latitude, if published.
    pub latitude: Option<f64>,
    /// Longitude, if published.
    pub longitude: Option<f64>,
    /// Height, if published.
    pub height: Option<f64>,
    /// Reachable host/IP (TCPServer/Backbone/I2P).
    pub reachable_on: Option<String>,
    /// Bind port (TCPServer/Backbone).
    pub port: Option<u64>,
    /// Radio frequency in Hz (RNode).
    pub frequency: Option<u64>,
    /// Radio bandwidth in Hz (RNode).
    pub bandwidth: Option<u64>,
    /// Spreading factor (RNode).
    pub spreadingfactor: Option<u64>,
    /// Coding rate (RNode).
    pub codingrate: Option<u64>,
    /// IFAC network name, published only when both IFAC fields are set.
    pub ifac_netname: Option<String>,
    /// IFAC network key, published only when both IFAC fields are set.
    pub ifac_netkey: Option<String>,
}

/// A validated discovery announce, surfaced to higher layers.
///
/// Mirrors the `info` dict Python's `InterfaceAnnounceHandler` passes to its
/// callback, minus the auto-connect/config-entry derivations that belong to
/// later sub-tasks. This is the event a received-and-validated announce
/// produces; sub-task (c) turns it into a persistent registry entry and (b)
/// drives auto-connect from it.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredInterface {
    /// Interface type name.
    pub interface_type: String,
    /// Whether the announcing node has transport enabled.
    pub transport: bool,
    /// Sanitised name, or `"Discovered {interface_type}"` when none was sent.
    pub name: String,
    /// The announcing node's transport identity hash.
    pub transport_id: [u8; TRUNCATED_HASHBYTES],
    /// The announcing network identity hash (the announce's identity).
    pub network_id: [u8; TRUNCATED_HASHBYTES],
    /// Realised stamp value (leading zero bits); `>= required_value`.
    pub value: u32,
    /// The 32-byte stamp.
    pub stamp: [u8; STAMP_SIZE],
    /// Latitude, if present.
    pub latitude: Option<f64>,
    /// Longitude, if present.
    pub longitude: Option<f64>,
    /// Height, if present.
    pub height: Option<f64>,
    /// Reachable host/IP, if present.
    pub reachable_on: Option<String>,
    /// Port, if present.
    pub port: Option<u64>,
    /// Radio frequency, if present.
    pub frequency: Option<u64>,
    /// Radio bandwidth, if present.
    pub bandwidth: Option<u64>,
    /// Spreading factor, if present.
    pub spreadingfactor: Option<u64>,
    /// Coding rate, if present.
    pub codingrate: Option<u64>,
    /// IFAC network name, if present.
    pub ifac_netname: Option<String>,
    /// IFAC network key, if present.
    pub ifac_netkey: Option<String>,
    /// Stable per-endpoint hash: `full_hash(hex(transport_id) + name)`.
    pub discovery_hash: [u8; STAMP_SIZE],
}

/// Sanitise a raw name for outbound encoding (Discovery.py `sanitize`):
/// strip carriage returns / newlines and surrounding whitespace.
fn sanitize(input: &str) -> String {
    input.replace(['\n', '\r'], "").trim().into()
}

/// Encode the integer-keyed `info` msgpack map for `desc`.
///
/// Returns `None` when the descriptor lacks a field its interface type
/// requires (matching Python, which aborts the announce in that case).
fn encode_info(
    desc: &InterfaceDescriptor,
    transport_id: &[u8; TRUNCATED_HASHBYTES],
    transport_enabled: bool,
) -> Option<Vec<u8>> {
    // Body is built first so the exact entry count is known for the map header.
    let mut body = Vec::new();
    let mut count: u32 = 0;

    // Base fields, in Python insertion order.
    msgpack::write_uint(&mut body, KEY_INTERFACE_TYPE);
    msgpack::write_str(&mut body, &desc.interface_type);
    count += 1;

    msgpack::write_uint(&mut body, KEY_TRANSPORT);
    msgpack::write_bool(&mut body, transport_enabled);
    count += 1;

    msgpack::write_uint(&mut body, KEY_TRANSPORT_ID);
    msgpack::write_bin(&mut body, transport_id);
    count += 1;

    msgpack::write_uint(&mut body, KEY_NAME);
    match &desc.name {
        Some(name) => msgpack::write_str(&mut body, &sanitize(name)),
        None => msgpack::write_nil(&mut body),
    }
    count += 1;

    for (key, value) in [
        (KEY_LATITUDE, desc.latitude),
        (KEY_LONGITUDE, desc.longitude),
        (KEY_HEIGHT, desc.height),
    ] {
        msgpack::write_uint(&mut body, key);
        match value {
            Some(v) => msgpack::write_float64(&mut body, v),
            None => msgpack::write_nil(&mut body),
        }
        count += 1;
    }

    // Type-specific fields, in Python order.
    match desc.interface_type.as_str() {
        "BackboneInterface" | "TCPServerInterface" => {
            msgpack::write_uint(&mut body, KEY_REACHABLE_ON);
            msgpack::write_str(&mut body, &sanitize(desc.reachable_on.as_deref()?));
            count += 1;
            msgpack::write_uint(&mut body, KEY_PORT);
            msgpack::write_uint(&mut body, desc.port?);
            count += 1;
        }
        "I2PInterface" => {
            // Only advertised when a reachable b32 address is known.
            if let Some(reachable) = desc.reachable_on.as_deref() {
                msgpack::write_uint(&mut body, KEY_REACHABLE_ON);
                msgpack::write_str(&mut body, &sanitize(reachable));
                count += 1;
            }
        }
        "RNodeInterface" => {
            for (key, value) in [
                (KEY_FREQUENCY, desc.frequency?),
                (KEY_BANDWIDTH, desc.bandwidth?),
                (KEY_SPREADINGFACTOR, desc.spreadingfactor?),
                (KEY_CODINGRATE, desc.codingrate?),
            ] {
                msgpack::write_uint(&mut body, key);
                msgpack::write_uint(&mut body, value);
                count += 1;
            }
        }
        _ => {}
    }

    // IFAC fields are published only when both are present (Discovery.py adds
    // them together under discovery_publish_ifac).
    if let (Some(netname), Some(netkey)) = (&desc.ifac_netname, &desc.ifac_netkey) {
        msgpack::write_uint(&mut body, KEY_IFAC_NETNAME);
        msgpack::write_str(&mut body, &sanitize(netname));
        count += 1;
        msgpack::write_uint(&mut body, KEY_IFAC_NETKEY);
        msgpack::write_str(&mut body, &sanitize(netkey));
        count += 1;
    }

    let mut out = Vec::with_capacity(1 + body.len());
    write_map_header(&mut out, count);
    out.extend_from_slice(&body);
    Some(out)
}

/// Write a msgpack map header (`umsgpack._pack_map`): fixmap for `< 16`
/// entries, otherwise map16.
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

/// Build the discovery announce `app_data` for one interface.
///
/// Produces `flags(0x00) + msgpack(info) + stamp(32)`, generating a fresh PoW
/// stamp at [`DEFAULT_STAMP_VALUE`]. This is the plaintext "emit" half: hand
/// the result to `Destination::announce(app_data = ...)` on the discovery
/// destination. For a private (encrypted) discovery network, use
/// [`build_announce_app_data_encrypted`] instead.
///
/// Returns `None` if the descriptor is missing a field its type requires.
pub fn build_announce_app_data(
    desc: &InterfaceDescriptor,
    transport_id: &[u8; TRUNCATED_HASHBYTES],
    transport_enabled: bool,
    rng: &mut impl CryptoRngCore,
) -> Option<Vec<u8>> {
    build_announce_app_data_inner(desc, transport_id, transport_enabled, None, rng)
}

/// Build an ENCRYPTED discovery announce `app_data` for a private discovery
/// network.
///
/// Produces `flags(FLAG_ENCRYPTED) + network_identity.encrypt(msgpack(info) + stamp(32))`.
/// The stamp is generated over the plaintext `msgpack(info)` exactly as in the
/// plaintext path, then the whole `msgpack(info) + stamp(32)` tail is encrypted
/// with `network_identity` (Python `Discovery.py` `get_interface_announce_data`:
/// `payload = self.owner.network_identity.encrypt(packed+stamp)`). Only nodes
/// holding the same network identity's private key can [decrypt and
/// decode][parse_announce_app_data_decrypt] the announce.
///
/// Returns `None` if the descriptor is missing a required field or encryption
/// fails.
pub fn build_announce_app_data_encrypted(
    desc: &InterfaceDescriptor,
    transport_id: &[u8; TRUNCATED_HASHBYTES],
    transport_enabled: bool,
    network_identity: &Identity,
    rng: &mut impl CryptoRngCore,
) -> Option<Vec<u8>> {
    build_announce_app_data_inner(
        desc,
        transport_id,
        transport_enabled,
        Some(network_identity),
        rng,
    )
}

/// Shared emit path for the plaintext and encrypted variants.
///
/// `network_identity == None` reproduces the sub-task (a) plaintext wire format
/// unchanged; `Some` sets `FLAG_ENCRYPTED` and encrypts the `packed + stamp`
/// tail (Python encrypts `packed+stamp` together, leaving the flags byte in the
/// clear).
fn build_announce_app_data_inner(
    desc: &InterfaceDescriptor,
    transport_id: &[u8; TRUNCATED_HASHBYTES],
    transport_enabled: bool,
    network_identity: Option<&Identity>,
    rng: &mut impl CryptoRngCore,
) -> Option<Vec<u8>> {
    let packed = encode_info(desc, transport_id, transport_enabled)?;
    let infohash = full_hash(&packed);
    // `DEFAULT_STAMP_VALUE` is a local constant well under the hash width, so
    // the guard cannot fire here; propagate rather than unwrap so it stays that
    // way if the cost ever becomes configurable.
    let (stamp, _value) =
        stamp::generate_stamp(&infohash, DEFAULT_STAMP_VALUE, WORKBLOCK_EXPAND_ROUNDS, rng).ok()?;

    let mut tail = Vec::with_capacity(packed.len() + STAMP_SIZE);
    tail.extend_from_slice(&packed);
    tail.extend_from_slice(&stamp);

    match network_identity {
        None => {
            let mut out = Vec::with_capacity(1 + tail.len());
            out.push(0x00); // unencrypted, unsigned
            out.extend_from_slice(&tail);
            Some(out)
        }
        Some(identity) => {
            let ciphertext = identity.encrypt(&tail, rng).ok()?;
            let mut out = Vec::with_capacity(1 + ciphertext.len());
            out.push(FLAG_ENCRYPTED);
            out.extend_from_slice(&ciphertext);
            Some(out)
        }
    }
}

/// Read a msgpack string, or `None` for msgpack nil.
fn read_str_or_nil(data: &[u8], pos: &mut usize) -> Option<Option<String>> {
    if data.get(*pos) == Some(&0xc0) {
        *pos += 1;
        return Some(None);
    }
    let bytes = msgpack::read_msgpack_str(data, pos)?;
    Some(Some(core::str::from_utf8(bytes).ok()?.into()))
}

/// Read a msgpack float, or `None` for msgpack nil.
fn read_float_or_nil(data: &[u8], pos: &mut usize) -> Option<Option<f64>> {
    if data.get(*pos) == Some(&0xc0) {
        *pos += 1;
        return Some(None);
    }
    Some(Some(msgpack::read_float64(data, pos)?))
}

/// Decode and validate a discovery announce `app_data`.
///
/// This is the "receive + validate + surface" half: recompute the workblock
/// from `full_hash(packed)`, check the stamp meets `required_value`, unpack the
/// `info` map, and return the [`DiscoveredInterface`] event. Returns `None`
/// when the stamp is invalid/insufficient, when the payload is malformed, or
/// when required fields are absent or the wrong type (mirroring Python, whose
/// handler swallows such announces).
///
/// `required_value` is a floor, not an equality: a stamp worth MORE than asked
/// for is accepted. Callers on the receive path should pass
/// [`MIN_REQUIRED_STAMP_VALUE`], not [`DEFAULT_STAMP_VALUE`] — the latter is
/// what we mint at, and using it to gate incoming announces would make us deaf
/// to peers still minting at the older cost.
///
/// `network_id` is the hash of the announce's own identity (the announcing
/// node). Encrypted announces (the `FLAG_ENCRYPTED` bit) cannot be decoded
/// without the network identity and return `None`; use
/// [`parse_announce_app_data_decrypt`] on a private discovery network.
pub fn parse_announce_app_data(
    app_data: &[u8],
    network_id: &[u8; TRUNCATED_HASHBYTES],
    required_value: u32,
) -> Option<DiscoveredInterface> {
    parse_announce_app_data_inner(app_data, network_id, required_value, None)
}

/// Decode and validate a discovery announce `app_data`, decrypting it with a
/// network identity when it is encrypted.
///
/// This is the receive half for a private (encrypted) discovery network. A
/// plaintext announce is decoded exactly as by [`parse_announce_app_data`]; an
/// encrypted announce (`FLAG_ENCRYPTED`) is first decrypted with
/// `network_identity` and then stamp-validated and parsed (Python `Discovery.py`
/// `received_announce`: `app_data = RNS.Transport.network_identity.decrypt(app_data)`
/// before the stamp check). An encrypted announce that does not decrypt under
/// this identity (a foreign network, or a tampered payload) returns `None` --
/// the security property that only shared-identity nodes can decode the
/// network.
pub fn parse_announce_app_data_decrypt(
    app_data: &[u8],
    network_id: &[u8; TRUNCATED_HASHBYTES],
    required_value: u32,
    network_identity: &Identity,
) -> Option<DiscoveredInterface> {
    parse_announce_app_data_inner(app_data, network_id, required_value, Some(network_identity))
}

/// Shared receive path for the plaintext and decrypting variants.
///
/// `network_identity == None` reproduces the sub-task (a) behaviour (encrypted
/// announces are rejected); `Some` decrypts the `FLAG_ENCRYPTED` tail before
/// stamp validation and parse.
fn parse_announce_app_data_inner(
    app_data: &[u8],
    network_id: &[u8; TRUNCATED_HASHBYTES],
    required_value: u32,
    network_identity: Option<&Identity>,
) -> Option<DiscoveredInterface> {
    if app_data.len() <= STAMP_SIZE + 1 {
        return None;
    }

    let flags = app_data[0];
    let rest = &app_data[1..];
    let _ = FLAG_SIGNED; // signed variant unused here

    // Decrypted bytes must outlive `body` when the encrypted path is taken.
    let decrypted;
    let body: &[u8] = if flags & FLAG_ENCRYPTED != 0 {
        // Python: `if not RNS.Transport.has_network_identity(): return`, then
        // `app_data = network_identity.decrypt(app_data); if not app_data: return`.
        let identity = network_identity?;
        decrypted = identity.decrypt(rest).ok()?;
        if decrypted.len() <= STAMP_SIZE {
            return None;
        }
        &decrypted
    } else {
        rest
    };

    let split = body.len() - STAMP_SIZE;
    let packed = &body[..split];
    let stamp: [u8; STAMP_SIZE] = body[split..].try_into().ok()?;

    let infohash = full_hash(packed);
    let workblock = stamp::stamp_workblock(&infohash, WORKBLOCK_EXPAND_ROUNDS);
    if !stamp::stamp_valid(&stamp, required_value, &workblock) {
        return None;
    }
    let value = stamp::stamp_value(&workblock, &stamp);
    if value < required_value {
        return None;
    }

    // Unpack the integer-keyed info map.
    let mut pos = 0usize;
    let count = read_map_len(packed, &mut pos)?;

    let mut interface_type: Option<String> = None;
    let mut transport: Option<bool> = None;
    let mut transport_id: Option<[u8; TRUNCATED_HASHBYTES]> = None;
    let mut name: Option<Option<String>> = None;
    let mut latitude: Option<Option<f64>> = None;
    let mut longitude: Option<Option<f64>> = None;
    let mut height: Option<Option<f64>> = None;
    let mut reachable_on: Option<String> = None;
    let mut port: Option<u64> = None;
    let mut frequency: Option<u64> = None;
    let mut bandwidth: Option<u64> = None;
    let mut spreadingfactor: Option<u64> = None;
    let mut codingrate: Option<u64> = None;
    let mut ifac_netname: Option<String> = None;
    let mut ifac_netkey: Option<String> = None;

    for _ in 0..count {
        let key = msgpack::read_msgpack_uint(packed, &mut pos)?;
        match key {
            KEY_INTERFACE_TYPE => {
                let bytes = msgpack::read_msgpack_str(packed, &mut pos)?;
                interface_type = Some(core::str::from_utf8(bytes).ok()?.into());
            }
            KEY_TRANSPORT => transport = Some(msgpack::read_bool(packed, &mut pos)?),
            KEY_TRANSPORT_ID => {
                let bytes = msgpack::read_msgpack_bin(packed, &mut pos)?;
                transport_id = Some(bytes.try_into().ok()?);
            }
            KEY_NAME => name = Some(read_str_or_nil(packed, &mut pos)?),
            KEY_LATITUDE => latitude = Some(read_float_or_nil(packed, &mut pos)?),
            KEY_LONGITUDE => longitude = Some(read_float_or_nil(packed, &mut pos)?),
            KEY_HEIGHT => height = Some(read_float_or_nil(packed, &mut pos)?),
            KEY_REACHABLE_ON => reachable_on = read_str_or_nil(packed, &mut pos)?,
            KEY_PORT => port = Some(msgpack::read_msgpack_uint(packed, &mut pos)?),
            KEY_FREQUENCY => frequency = Some(msgpack::read_msgpack_uint(packed, &mut pos)?),
            KEY_BANDWIDTH => bandwidth = Some(msgpack::read_msgpack_uint(packed, &mut pos)?),
            KEY_SPREADINGFACTOR => {
                spreadingfactor = Some(msgpack::read_msgpack_uint(packed, &mut pos)?)
            }
            KEY_CODINGRATE => codingrate = Some(msgpack::read_msgpack_uint(packed, &mut pos)?),
            KEY_IFAC_NETNAME => ifac_netname = read_str_or_nil(packed, &mut pos)?,
            KEY_IFAC_NETKEY => ifac_netkey = read_str_or_nil(packed, &mut pos)?,
            // Weave/KISS radio specifics (deferred to sub-task (b)) and the
            // 1.5.0 operator LXMF address: recognised on the wire but not yet
            // surfaced; skip their values for now.
            KEY_MODULATION | KEY_CHANNEL | KEY_OP_ADDR => {
                msgpack::skip_msgpack_value(packed, &mut pos)?
            }
            // Any other unknown key: skip for forward compatibility.
            _ => msgpack::skip_msgpack_value(packed, &mut pos)?,
        }
    }

    // Required base fields and their types (Discovery.py received_announce).
    let interface_type = interface_type?;
    if !DISCOVERABLE_INTERFACE_TYPES.contains(&interface_type.as_str()) {
        return None;
    }
    let transport = transport?;
    let transport_id = transport_id?;
    // NAME / LATITUDE / LONGITUDE / HEIGHT must be present (nil is allowed).
    let raw_name = name?;
    let latitude = latitude?;
    let longitude = longitude?;
    let height = height?;

    let sanitized = raw_name.as_deref().and_then(sanitize_name);
    let name = sanitized.unwrap_or_else(|| format!("Discovered {interface_type}"));

    let discovery_hash = compute_discovery_hash(&transport_id, &name);

    Some(DiscoveredInterface {
        interface_type,
        transport,
        name,
        transport_id,
        network_id: *network_id,
        value,
        stamp,
        latitude,
        longitude,
        height,
        reachable_on,
        port,
        frequency,
        bandwidth,
        spreadingfactor,
        codingrate,
        ifac_netname,
        ifac_netkey,
        discovery_hash,
    })
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

/// Stable per-endpoint hash: `full_hash((hex(transport_id) + name).utf8)`
/// (Discovery.py `discovery_hash_material`).
fn compute_discovery_hash(
    transport_id: &[u8; TRUNCATED_HASHBYTES],
    name: &str,
) -> [u8; STAMP_SIZE] {
    let mut material = String::with_capacity(TRUNCATED_HASHBYTES * 2 + name.len());
    for byte in transport_id.iter() {
        material.push_str(&format!("{byte:02x}"));
    }
    material.push_str(name);
    full_hash(material.as_bytes())
}

/// Sanitise a received name (Discovery.py `sanitize_name`).
///
/// ASCII-only, whitespace-collapsed, and trimmed of non-alphanumeric edges.
/// Returns `None` when nothing survives (Python then falls back to the
/// `"Discovered {type}"` default).
fn sanitize_name(name: &str) -> Option<String> {
    // Keep ASCII only (Python: encode("ascii","ignore")), then strip.
    let mut s: String = name.chars().filter(|c| c.is_ascii()).collect();
    s = s.trim().into();

    // Collapse runs of spaces (Python: one replace for 5-, then 3-, then
    // 2-space runs; `str::replace` replaces all non-overlapping occurrences).
    for width in [5usize, 3, 2] {
        let mut sep = String::with_capacity(width);
        for _ in 0..width {
            sep.push(' ');
        }
        s = s.replace(&sep, " ");
    }

    // Trim leading chars that are not [0-9A-Za-z].
    while let Some(c) = s.chars().next() {
        if c.is_ascii_alphanumeric() {
            break;
        }
        s.remove(0);
    }
    // Trim trailing chars not in [0-9A-Za-z] plus ')'.
    while let Some(c) = s.chars().last() {
        if c.is_ascii_alphanumeric() || c == ')' {
            break;
        }
        s.pop();
    }

    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests;
