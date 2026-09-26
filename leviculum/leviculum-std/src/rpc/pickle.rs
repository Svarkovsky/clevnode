//! RPC request/response serialization (pickle + msgpack).
//!
//! RNS migrated its shared-instance RPC codec from Python `pickle` to
//! `RNS.vendor.umsgpack` (standard msgpack). To interoperate with both legacy
//! (pickle) and current (msgpack, RNS 1.3.x+) Python-RNS peers, requests are
//! decoded by sniffing the leading byte, and the response is encoded back in
//! the same codec the request arrived in.
//!
//! Internally, requests parse into the typed [`RpcRequest`] enum and handlers
//! build responses as a `serde_pickle::Value` tree (a convenient tagged value
//! model); for msgpack peers that tree is transcoded to `rmpv::Value`.

use serde_pickle::value::{HashableValue, Value};
use std::collections::BTreeMap;

use super::error::RpcError;

/// Which wire codec a peer speaks for shared-instance RPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Codec {
    /// Python `pickle` — legacy RNS. The request stream begins with the `0x80`
    /// PROTO opcode (protocol >= 2, which every modern pickle uses).
    Pickle,
    /// msgpack (`RNS.vendor.umsgpack`) — RNS 1.3.x and later.
    Msgpack,
}

/// Sniff the request codec from the leading byte.
///
/// Every modern pickle stream (protocol >= 2) begins with the `0x80` PROTO
/// opcode. A msgpack RPC request is a *non-empty* map, whose header is
/// `0x81..=0x8f` (fixmap) or `0xde`/`0xdf` (map16/map32) — never `0x80`, which
/// is an empty map (no RPC request is empty). So a leading `0x80` unambiguously
/// means pickle.
fn detect_codec(data: &[u8]) -> Codec {
    match data.first() {
        Some(0x80) => Codec::Pickle,
        _ => Codec::Msgpack,
    }
}

/// Parsed RPC request.
///
/// Fields are parsed from pickle dicts and logged via `Debug`.
/// The `until`/`reason` blackhole params are parsed and logged but not yet
/// acted on (Stage 2 will honor expiry — see handlers.rs).
// `PartialEq` only: the blackhole `until` is an f64 deadline, so `Eq` is not
// available on this enum.
#[derive(Debug, PartialEq)]
#[allow(dead_code)] // until/reason parsed for the wire contract, not yet acted on
pub(crate) enum RpcRequest {
    // GET commands
    GetInterfaceStats,
    /// Number of entries in the TRANSPORT link table — the links this node
    /// relays, not the ones it terminates (`Reticulum.get_link_count` →
    /// `Transport.link_count()` → `len(Transport.link_table)`, Reticulum
    /// 1.5.2). `rnstatus -l` renders it as "N entries in link table".
    GetLinkCount,
    /// Local link inventory — Leviculum-only extension; no Python `rnsd`
    /// precedent (Python only exposes `link_count`, which counts a different
    /// table: the relayed links, not these terminated ones). Response is a
    /// list of dicts; see `LinkTableExport` for the per-row shape.
    GetLinkTable,
    GetPathTable {
        max_hops: Option<i64>,
    },
    /// Structured dump of the tables the transport maintains (Codeberg #174) —
    /// a Leviculum-only extension with no Python `rnsd` precedent. Python's
    /// `rpc_loop` matches no arm for it and closes the connection without a
    /// reply (Reticulum.py:1213-1260), so a client asking an `rnsd` sees a
    /// transport error, never a hang. Response shape: see
    /// `build_transport_tables`.
    GetTransportTables,
    /// Identity listing (`lnstatus --identities`) — a Leviculum-only extension
    /// with no Python `rnsd` precedent: every identity the daemon has learned
    /// from announces, with the announced destination and the live path toward
    /// it. Like `transport_tables`, Python's `rpc_loop` matches no arm for it
    /// and closes the connection without a reply, so a client asking an `rnsd`
    /// sees a transport error, never a hang. Response shape: see
    /// `build_identity_table`.
    GetIdentityTable,
    /// Discovered-interface registry query (Codeberg #32). Response is a list of
    /// per-record dicts (see `build_discovered_interfaces`), mirroring Python
    /// `RNS.Discovery.InterfaceDiscovery.list_discovered_interfaces`.
    GetDiscoveredInterfaces,
    GetRateTable,
    GetNextHop {
        destination_hash: Vec<u8>,
    },
    GetNextHopIfName {
        destination_hash: Vec<u8>,
    },
    GetFirstHopTimeout {
        destination_hash: Vec<u8>,
    },
    GetPacketRssi {
        packet_hash: Vec<u8>,
    },
    GetPacketSnr {
        packet_hash: Vec<u8>,
    },
    GetPacketQ {
        packet_hash: Vec<u8>,
    },
    /// Slowest online interface bitrate in bits per second, or `None` when no
    /// online interface reports one (Reticulum 1.5.2
    /// `Reticulum.get_lowest_interface_bitrate`, answering from
    /// `Transport.lowest_interface_bitrate`; the verb postdates our pinned
    /// 1.3.5 reference, which is why no line citation resolves for it).
    GetLowestInterfaceBitrate,
    /// Round-trip estimate for an MTU on the slowest online interface plus per
    /// hop grace, in seconds; 0 when no bitrate is known
    /// (Reticulum 1.5.2 `Transport.medium_path_timeout`). rnprobe,
    /// rnpath and rncp size their wait window with it.
    GetMediumPathTimeout,
    /// Count of the TRANSPORT link-table entries that are validated
    /// (Reticulum 1.5.2 `Transport.active_link_count`). `rnstatus -l`
    /// renders it as the `(N active)` suffix.
    ///
    /// Upstream's implementation does not compute that: it iterates the
    /// link table's keys and indexes `entry[IDX_LT_VALIDATED]` on the
    /// 16-byte link id, so it counts ids whose eighth byte is non-zero
    /// (Transport.py:3215-3216, 1.5.2). We answer the documented meaning —
    /// see the handler for why.
    GetActiveLinkCount,
    GetBlackholedIdentities,
    /// Membership check against the blackhole set. Python sends
    /// `{"get": "is_blackholed", "identity_hash": <16 bytes>}` and expects a
    /// bool back (Reticulum.py:1708-1720).
    IsBlackholed {
        identity_hash: Vec<u8>,
    },
    // DROP commands
    DropPath {
        destination_hash: Vec<u8>,
    },
    DropAllVia {
        destination_hash: Vec<u8>,
    },
    DropAnnounceQueues,
    // BLACKHOLE commands
    BlackholeIdentity {
        identity_hash: Vec<u8>,
        until: Option<f64>,
        reason: Option<String>,
    },
    UnblackholeIdentity {
        identity_hash: Vec<u8>,
    },
    // destination_data lifecycle commands (stubs — see handlers.rs)
    DestinationDataUsed {
        destination_hash: Vec<u8>,
    },
    DestinationDataRetain {
        destination_hash: Vec<u8>,
    },
    DestinationDataUnretain {
        destination_hash: Vec<u8>,
    },
    // identity_data lifecycle commands (stubs — see handlers.rs)
    IdentityDataRetain {
        identity_hash: Vec<u8>,
    },
    IdentityDataUnretain {
        identity_hash: Vec<u8>,
    },
}

/// Parse an RPC request, auto-detecting pickle vs msgpack.
///
/// Returns the typed request together with the codec it arrived in, so the
/// caller can encode the response to match (see [`serialize_response`]).
pub(crate) fn parse_request(data: &[u8]) -> Result<(RpcRequest, Codec), RpcError> {
    let codec = detect_codec(data);
    let value = match codec {
        Codec::Pickle => serde_pickle::value_from_slice(data, Default::default())
            .map_err(|e| RpcError::Pickle(format!("deserialize: {}", e)))?,
        Codec::Msgpack => msgpack_to_value(data)?,
    };
    Ok((request_from_value(value)?, codec))
}

/// Dispatch a decoded request dict (codec-agnostic) into a typed [`RpcRequest`].
fn request_from_value(value: Value) -> Result<RpcRequest, RpcError> {
    let dict = match value {
        Value::Dict(d) => d,
        _ => return Err(RpcError::InvalidFormat("expected dict".into())),
    };

    // Check for "get" key
    if let Some(get_val) = dict_get_str(&dict, "get") {
        return match get_val.as_str() {
            "interface_stats" => Ok(RpcRequest::GetInterfaceStats),
            "link_count" => Ok(RpcRequest::GetLinkCount),
            "link_table" => Ok(RpcRequest::GetLinkTable),
            "path_table" => {
                let max_hops = dict_get_int(&dict, "max_hops");
                Ok(RpcRequest::GetPathTable { max_hops })
            }
            "transport_tables" => Ok(RpcRequest::GetTransportTables),
            "identities" => Ok(RpcRequest::GetIdentityTable),
            "discovered_interfaces" => Ok(RpcRequest::GetDiscoveredInterfaces),
            "rate_table" => Ok(RpcRequest::GetRateTable),
            "next_hop" => {
                let destination_hash = dict_get_bytes(&dict, "destination_hash")
                    .ok_or_else(|| RpcError::InvalidFormat("missing destination_hash".into()))?;
                Ok(RpcRequest::GetNextHop { destination_hash })
            }
            "next_hop_if_name" => {
                let destination_hash = dict_get_bytes(&dict, "destination_hash")
                    .ok_or_else(|| RpcError::InvalidFormat("missing destination_hash".into()))?;
                Ok(RpcRequest::GetNextHopIfName { destination_hash })
            }
            "first_hop_timeout" => {
                let destination_hash = dict_get_bytes(&dict, "destination_hash")
                    .ok_or_else(|| RpcError::InvalidFormat("missing destination_hash".into()))?;
                Ok(RpcRequest::GetFirstHopTimeout { destination_hash })
            }
            "packet_rssi" => {
                let packet_hash = dict_get_bytes(&dict, "packet_hash")
                    .ok_or_else(|| RpcError::InvalidFormat("missing packet_hash".into()))?;
                Ok(RpcRequest::GetPacketRssi { packet_hash })
            }
            "packet_snr" => {
                let packet_hash = dict_get_bytes(&dict, "packet_hash")
                    .ok_or_else(|| RpcError::InvalidFormat("missing packet_hash".into()))?;
                Ok(RpcRequest::GetPacketSnr { packet_hash })
            }
            "packet_q" => {
                let packet_hash = dict_get_bytes(&dict, "packet_hash")
                    .ok_or_else(|| RpcError::InvalidFormat("missing packet_hash".into()))?;
                Ok(RpcRequest::GetPacketQ { packet_hash })
            }
            "lowest_interface_bitrate" => Ok(RpcRequest::GetLowestInterfaceBitrate),
            "medium_path_timeout" => Ok(RpcRequest::GetMediumPathTimeout),
            "active_link_count" => Ok(RpcRequest::GetActiveLinkCount),
            "blackholed_identities" => Ok(RpcRequest::GetBlackholedIdentities),
            "is_blackholed" => {
                let identity_hash = dict_get_bytes(&dict, "identity_hash")
                    .ok_or_else(|| RpcError::InvalidFormat("missing identity_hash".into()))?;
                Ok(RpcRequest::IsBlackholed { identity_hash })
            }
            other => Err(RpcError::InvalidFormat(format!(
                "unknown get command: {}",
                other
            ))),
        };
    }

    // Check for "drop" key
    if let Some(drop_val) = dict_get_str(&dict, "drop") {
        return match drop_val.as_str() {
            "path" => {
                let destination_hash = dict_get_bytes(&dict, "destination_hash")
                    .ok_or_else(|| RpcError::InvalidFormat("missing destination_hash".into()))?;
                Ok(RpcRequest::DropPath { destination_hash })
            }
            "all_via" => {
                let destination_hash = dict_get_bytes(&dict, "destination_hash")
                    .ok_or_else(|| RpcError::InvalidFormat("missing destination_hash".into()))?;
                Ok(RpcRequest::DropAllVia { destination_hash })
            }
            "announce_queues" => Ok(RpcRequest::DropAnnounceQueues),
            other => Err(RpcError::InvalidFormat(format!(
                "unknown drop command: {}",
                other
            ))),
        };
    }

    // Check for "blackhole_identity" key
    if let Some(identity_bytes) = dict_get_bytes(&dict, "blackhole_identity") {
        let until = dict_get_float(&dict, "until");
        let reason = dict_get_str(&dict, "reason");
        return Ok(RpcRequest::BlackholeIdentity {
            identity_hash: identity_bytes,
            until,
            reason,
        });
    }

    // Check for "unblackhole_identity" key
    if let Some(identity_bytes) = dict_get_bytes(&dict, "unblackhole_identity") {
        return Ok(RpcRequest::UnblackholeIdentity {
            identity_hash: identity_bytes,
        });
    }

    // Check for "destination_data" key (used / retain / unretain operations)
    if let Some(op) = dict_get_str(&dict, "destination_data") {
        let destination_hash = dict_get_bytes(&dict, "destination_hash")
            .ok_or_else(|| RpcError::InvalidFormat("missing destination_hash".into()))?;
        return match op.as_str() {
            "used" => Ok(RpcRequest::DestinationDataUsed { destination_hash }),
            "retain" => Ok(RpcRequest::DestinationDataRetain { destination_hash }),
            "unretain" => Ok(RpcRequest::DestinationDataUnretain { destination_hash }),
            other => Err(RpcError::InvalidFormat(format!(
                "unknown destination_data operation: {}",
                other
            ))),
        };
    }

    // Check for "identity_data" key (retain / unretain operations).
    // Upstream rnid issues `{"identity_data": "retain", "identity_hash": <bytes>}`
    // via Reticulum._retain_identity (Reticulum.py:1316). Parsed identically to
    // destination_data, reading the `identity_hash` bytes field.
    if let Some(op) = dict_get_str(&dict, "identity_data") {
        let identity_hash = dict_get_bytes(&dict, "identity_hash")
            .ok_or_else(|| RpcError::InvalidFormat("missing identity_hash".into()))?;
        return match op.as_str() {
            "retain" => Ok(RpcRequest::IdentityDataRetain { identity_hash }),
            "unretain" => Ok(RpcRequest::IdentityDataUnretain { identity_hash }),
            other => Err(RpcError::InvalidFormat(format!(
                "unknown identity_data operation: {}",
                other
            ))),
        };
    }

    Err(RpcError::InvalidFormat("unrecognized request".into()))
}

/// Serialize an RPC response value in the given codec.
pub(crate) fn serialize_response(value: &Value, codec: Codec) -> Result<Vec<u8>, RpcError> {
    match codec {
        Codec::Pickle => serde_pickle::value_to_vec(value, Default::default())
            .map_err(|e| RpcError::Pickle(format!("serialize: {}", e))),
        Codec::Msgpack => {
            let mv = value_to_rmpv(value)?;
            let mut buf = Vec::new();
            rmpv::encode::write_value(&mut buf, &mv)
                .map_err(|e| RpcError::InvalidFormat(format!("msgpack encode: {}", e)))?;
            Ok(buf)
        }
    }
}

/// Encode a shared-instance RPC request (a `Value` dict) as msgpack, matching
/// what upstream Python-RNS now sends on the wire (`conn.send_bytes(mp.packb(req))`,
/// RNS/Reticulum.py). Reuses the response transcode path, so str keys/values
/// (`"get"`, command names) become msgpack `str` and byte-string values (hashes)
/// become msgpack `bin` — exactly what `umsgpack` expects to decode back into
/// Python `str`/`bytes`.
pub(crate) fn encode_request_msgpack(request: &Value) -> Result<Vec<u8>, RpcError> {
    let mv = value_to_rmpv(request)?;
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &mv)
        .map_err(|e| RpcError::InvalidFormat(format!("msgpack encode: {}", e)))?;
    Ok(buf)
}

/// Decode a msgpack RPC response (`mp.unpackb(conn.recv_bytes())`) into the
/// shared `Value` tree the printing/json code already consumes.
pub(crate) fn decode_response_msgpack(data: &[u8]) -> Result<Value, RpcError> {
    msgpack_to_value(data)
}

// Codec transcoding: handlers and the request dispatcher both work in terms of
// `serde_pickle::Value`; these bridge that model to/from `rmpv::Value` so the
// msgpack path reuses the exact same request parsing and response builders.

/// Decode msgpack bytes into the shared `serde_pickle::Value` tree.
fn msgpack_to_value(data: &[u8]) -> Result<Value, RpcError> {
    let mv = rmpv::decode::read_value(&mut &data[..])
        .map_err(|e| RpcError::InvalidFormat(format!("msgpack decode: {}", e)))?;
    rmpv_to_value(&mv)
}

fn rmpv_to_value(v: &rmpv::Value) -> Result<Value, RpcError> {
    use rmpv::Value as M;
    Ok(match v {
        M::Nil => Value::None,
        M::Boolean(b) => Value::Bool(*b),
        M::Integer(i) => {
            if let Some(n) = i.as_i64() {
                Value::I64(n)
            } else if let Some(n) = i.as_u64() {
                Value::I64(n as i64)
            } else {
                return Err(RpcError::InvalidFormat(
                    "msgpack integer out of range".into(),
                ));
            }
        }
        M::F32(f) => Value::F64(*f as f64),
        M::F64(f) => Value::F64(*f),
        M::String(s) => Value::String(
            s.as_str()
                .ok_or_else(|| RpcError::InvalidFormat("non-utf8 msgpack string".into()))?
                .to_string(),
        ),
        M::Binary(b) => Value::Bytes(b.clone()),
        M::Array(a) => Value::List(a.iter().map(rmpv_to_value).collect::<Result<_, _>>()?),
        M::Map(m) => {
            let mut dict = BTreeMap::new();
            for (k, val) in m {
                dict.insert(rmpv_to_hashable(k)?, rmpv_to_value(val)?);
            }
            Value::Dict(dict)
        }
        M::Ext(..) => {
            return Err(RpcError::InvalidFormat(
                "unexpected msgpack ext type".into(),
            ))
        }
    })
}

fn rmpv_to_hashable(v: &rmpv::Value) -> Result<HashableValue, RpcError> {
    use rmpv::Value as M;
    Ok(match v {
        M::String(s) => HashableValue::String(
            s.as_str()
                .ok_or_else(|| RpcError::InvalidFormat("non-utf8 msgpack key".into()))?
                .to_string(),
        ),
        M::Binary(b) => HashableValue::Bytes(b.clone()),
        M::Boolean(b) => HashableValue::Bool(*b),
        M::Integer(i) => {
            HashableValue::I64(i.as_i64().ok_or_else(|| {
                RpcError::InvalidFormat("msgpack integer key out of range".into())
            })?)
        }
        _ => {
            return Err(RpcError::InvalidFormat(
                "unsupported msgpack map key".into(),
            ))
        }
    })
}

/// Transcode a `serde_pickle::Value` response tree into `rmpv::Value`.
///
/// Preserves the str-vs-bytes distinction RNS relies on: pickle strings become
/// msgpack `str` and pickle bytes become msgpack `bin`, so umsgpack decodes
/// them back to `str`/`bytes` respectively.
pub(crate) fn value_to_rmpv(v: &Value) -> Result<rmpv::Value, RpcError> {
    use rmpv::Value as M;
    Ok(match v {
        Value::None => M::Nil,
        Value::Bool(b) => M::Boolean(*b),
        Value::I64(n) => M::Integer((*n).into()),
        Value::F64(f) => M::F64(*f),
        Value::String(s) => M::String(s.clone().into()),
        Value::Bytes(b) => M::Binary(b.clone()),
        Value::List(items) | Value::Tuple(items) => {
            M::Array(items.iter().map(value_to_rmpv).collect::<Result<_, _>>()?)
        }
        Value::Dict(d) => {
            let mut pairs = Vec::with_capacity(d.len());
            for (k, val) in d {
                pairs.push((hashable_to_rmpv(k)?, value_to_rmpv(val)?));
            }
            M::Map(pairs)
        }
        other => {
            return Err(RpcError::InvalidFormat(format!(
                "cannot msgpack-encode response value: {:?}",
                other
            )))
        }
    })
}

fn hashable_to_rmpv(v: &HashableValue) -> Result<rmpv::Value, RpcError> {
    use rmpv::Value as M;
    Ok(match v {
        HashableValue::String(s) => M::String(s.clone().into()),
        HashableValue::Bytes(b) => M::Binary(b.clone()),
        HashableValue::Bool(b) => M::Boolean(*b),
        HashableValue::I64(n) => M::Integer((*n).into()),
        other => {
            return Err(RpcError::InvalidFormat(format!(
                "unsupported msgpack map key: {:?}",
                other
            )))
        }
    })
}

// Pickle dict helpers
/// Create a pickle string key
pub(crate) fn pickle_str_key(s: &str) -> HashableValue {
    HashableValue::String(s.into())
}

/// Create a pickle string value
pub(crate) fn pickle_str(s: &str) -> Value {
    Value::String(s.into())
}

/// Create a pickle bytes value
pub(crate) fn pickle_bytes(b: &[u8]) -> Value {
    Value::Bytes(b.to_vec())
}

/// Create a pickle int value
pub(crate) fn pickle_int(n: i64) -> Value {
    Value::I64(n)
}

/// Create a pickle float value
pub(crate) fn pickle_float(f: f64) -> Value {
    Value::F64(f)
}

/// Create a pickle bool value
pub(crate) fn pickle_bool(b: bool) -> Value {
    Value::Bool(b)
}

/// Create a pickle None value
pub(crate) fn pickle_none() -> Value {
    Value::None
}

/// Build a pickle dict from key-value pairs
pub(crate) fn pickle_dict(entries: Vec<(HashableValue, Value)>) -> Value {
    Value::Dict(BTreeMap::from_iter(entries))
}

/// Build a pickle list
pub(crate) fn pickle_list(items: Vec<Value>) -> Value {
    Value::List(items)
}

// Internal helpers
fn dict_get_str(dict: &BTreeMap<HashableValue, Value>, key: &str) -> Option<String> {
    let k = HashableValue::String(key.into());
    match dict.get(&k) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn dict_get_bytes(dict: &BTreeMap<HashableValue, Value>, key: &str) -> Option<Vec<u8>> {
    let k = HashableValue::String(key.into());
    match dict.get(&k) {
        Some(Value::Bytes(b)) => Some(b.clone()),
        _ => None,
    }
}

fn dict_get_int(dict: &BTreeMap<HashableValue, Value>, key: &str) -> Option<i64> {
    let k = HashableValue::String(key.into());
    match dict.get(&k) {
        Some(Value::I64(n)) => Some(*n),
        Some(Value::None) => None,
        _ => None,
    }
}

fn dict_get_float(dict: &BTreeMap<HashableValue, Value>, key: &str) -> Option<f64> {
    let k = HashableValue::String(key.into());
    match dict.get(&k) {
        Some(Value::F64(f)) => Some(*f),
        Some(Value::None) => None,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_get_request(command: &str) -> Vec<u8> {
        let dict = pickle_dict(vec![(pickle_str_key("get"), pickle_str(command))]);
        serde_pickle::value_to_vec(&dict, Default::default()).unwrap()
    }

    fn build_get_request_with_bytes(command: &str, key: &str, value: &[u8]) -> Vec<u8> {
        let dict = pickle_dict(vec![
            (pickle_str_key("get"), pickle_str(command)),
            (pickle_str_key(key), pickle_bytes(value)),
        ]);
        serde_pickle::value_to_vec(&dict, Default::default()).unwrap()
    }

    #[test]
    fn test_parse_get_interface_stats() {
        let data = build_get_request("interface_stats");
        let req = parse_request(&data).unwrap().0;
        assert!(matches!(req, RpcRequest::GetInterfaceStats));
    }

    #[test]
    fn test_parse_get_discovered_interfaces_pickle() {
        let data = build_get_request("discovered_interfaces");
        let (req, codec) = parse_request(&data).unwrap();
        assert_eq!(codec, Codec::Pickle);
        assert!(matches!(req, RpcRequest::GetDiscoveredInterfaces));
    }

    #[test]
    fn test_parse_get_discovered_interfaces_msgpack() {
        let data = msgpack_get_request("discovered_interfaces");
        let (req, codec) = parse_request(&data).unwrap();
        assert_eq!(codec, Codec::Msgpack);
        assert!(matches!(req, RpcRequest::GetDiscoveredInterfaces));
    }

    #[test]
    fn test_parse_get_link_count() {
        let data = build_get_request("link_count");
        let req = parse_request(&data).unwrap().0;
        assert!(matches!(req, RpcRequest::GetLinkCount));
    }

    /// The three verbs Codeberg #329 added, in BOTH codecs: a 1.5.2 client
    /// packs msgpack (`mp.packb` in every `Reticulum.get_*` RPC arm), older
    /// tooling and our
    /// own pickle path still speak pickle, and the dispatcher is shared.
    #[test]
    fn test_parse_the_timing_verbs_in_both_codecs() {
        for command in [
            "lowest_interface_bitrate",
            "medium_path_timeout",
            "active_link_count",
        ] {
            let (pickled, pickle_codec) = parse_request(&build_get_request(command)).unwrap();
            assert_eq!(pickle_codec, Codec::Pickle);
            let (packed, msgpack_codec) = parse_request(&msgpack_get_request(command)).unwrap();
            assert_eq!(msgpack_codec, Codec::Msgpack);
            let expected = match command {
                "lowest_interface_bitrate" => RpcRequest::GetLowestInterfaceBitrate,
                "medium_path_timeout" => RpcRequest::GetMediumPathTimeout,
                _ => RpcRequest::GetActiveLinkCount,
            };
            assert_eq!(pickled, expected, "pickle arm for {command}");
            assert_eq!(packed, expected, "msgpack arm for {command}");
        }
    }

    /// The identities listing verb (`lnstatus --identities`), in both codecs;
    /// the negative-control test below is what makes "it parsed" meaningful.
    #[test]
    fn test_parse_identities_in_both_codecs() {
        let (pickled, pickle_codec) = parse_request(&build_get_request("identities")).unwrap();
        assert_eq!(pickle_codec, Codec::Pickle);
        assert_eq!(pickled, RpcRequest::GetIdentityTable);
        let (packed, msgpack_codec) = parse_request(&msgpack_get_request("identities")).unwrap();
        assert_eq!(msgpack_codec, Codec::Msgpack);
        assert_eq!(packed, RpcRequest::GetIdentityTable);
    }

    /// Positive control for the test above: the dispatcher rejects what it
    /// does not know, so "it parsed" is evidence the arm exists.
    /// `profiling_results` is here by intent — it is DECLINED (see the verb
    /// dispatch in handlers.rs), and this pins that decision.
    #[test]
    fn test_declined_and_unknown_get_commands_do_not_parse() {
        for command in ["profiling_results", "medium_path_timeoutx", "not_a_verb"] {
            assert!(
                parse_request(&build_get_request(command)).is_err(),
                "{command} must not resolve to a request"
            );
            assert!(
                parse_request(&msgpack_get_request(command)).is_err(),
                "{command} must not resolve to a request (msgpack)"
            );
        }
    }

    #[test]
    fn test_parse_get_path_table() {
        let dict = pickle_dict(vec![
            (pickle_str_key("get"), pickle_str("path_table")),
            (pickle_str_key("max_hops"), pickle_int(5)),
        ]);
        let data = serde_pickle::value_to_vec(&dict, Default::default()).unwrap();
        let req = parse_request(&data).unwrap().0;
        match req {
            RpcRequest::GetPathTable { max_hops } => assert_eq!(max_hops, Some(5)),
            other => panic!("expected GetPathTable, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_get_next_hop() {
        let hash = vec![0xAB; 16];
        let data = build_get_request_with_bytes("next_hop", "destination_hash", &hash);
        let req = parse_request(&data).unwrap().0;
        match req {
            RpcRequest::GetNextHop { destination_hash } => assert_eq!(destination_hash, hash),
            other => panic!("expected GetNextHop, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_drop_path() {
        let hash = vec![0xCD; 16];
        let dict = pickle_dict(vec![
            (pickle_str_key("drop"), pickle_str("path")),
            (pickle_str_key("destination_hash"), pickle_bytes(&hash)),
        ]);
        let data = serde_pickle::value_to_vec(&dict, Default::default()).unwrap();
        let req = parse_request(&data).unwrap().0;
        match req {
            RpcRequest::DropPath { destination_hash } => assert_eq!(destination_hash, hash),
            other => panic!("expected DropPath, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_blackhole_identity() {
        let hash = vec![0xEF; 16];
        let dict = pickle_dict(vec![
            (pickle_str_key("blackhole_identity"), pickle_bytes(&hash)),
            (pickle_str_key("until"), pickle_float(1234567.0)),
            (pickle_str_key("reason"), pickle_str("testing")),
        ]);
        let data = serde_pickle::value_to_vec(&dict, Default::default()).unwrap();
        let req = parse_request(&data).unwrap().0;
        match req {
            RpcRequest::BlackholeIdentity {
                identity_hash,
                until,
                reason,
            } => {
                assert_eq!(identity_hash, hash);
                assert_eq!(until, Some(1234567.0));
                assert_eq!(reason, Some("testing".into()));
            }
            other => panic!("expected BlackholeIdentity, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_is_blackholed_pickle() {
        // Python sends {"get": "is_blackholed", "identity_hash": <16 bytes>}.
        let hash = vec![0x3Cu8; 16];
        let data = build_get_request_with_bytes("is_blackholed", "identity_hash", &hash);
        let (req, codec) = parse_request(&data).unwrap();
        assert_eq!(codec, Codec::Pickle);
        match req {
            RpcRequest::IsBlackholed { identity_hash } => assert_eq!(identity_hash, hash),
            other => panic!("expected IsBlackholed, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_is_blackholed_msgpack() {
        // Exactly what a 1.3.5 client sends: the hash as a msgpack bin.
        let hash = vec![0x3Cu8; 16];
        let v = rmpv::Value::Map(vec![
            (rmpv::Value::from("get"), rmpv::Value::from("is_blackholed")),
            (
                rmpv::Value::from("identity_hash"),
                rmpv::Value::Binary(hash.clone()),
            ),
        ]);
        let mut data = Vec::new();
        rmpv::encode::write_value(&mut data, &v).unwrap();
        let (req, codec) = parse_request(&data).unwrap();
        assert_eq!(codec, Codec::Msgpack);
        match req {
            RpcRequest::IsBlackholed { identity_hash } => assert_eq!(identity_hash, hash),
            other => panic!("expected IsBlackholed, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_is_blackholed_missing_hash_errors() {
        // is_blackholed without identity_hash is a clean parse error, never a panic.
        let data = build_get_request("is_blackholed");
        assert!(parse_request(&data).is_err());
    }

    #[test]
    fn test_parse_unknown_get_command() {
        let data = build_get_request("nonexistent");
        let result = parse_request(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_serialize_response_round_trip() {
        let response = pickle_dict(vec![
            (pickle_str_key("transport_id"), pickle_bytes(&[0x42; 16])),
            (pickle_str_key("transport_uptime"), pickle_float(123.456)),
            (pickle_str_key("interfaces"), pickle_list(vec![])),
        ]);
        let bytes = serialize_response(&response, Codec::Pickle).unwrap();
        let parsed: Value = serde_pickle::value_from_slice(&bytes, Default::default()).unwrap();
        match parsed {
            Value::Dict(d) => {
                assert!(d.contains_key(&pickle_str_key("transport_id")));
                assert!(d.contains_key(&pickle_str_key("transport_uptime")));
            }
            _ => panic!("expected dict"),
        }
    }

    // msgpack codec (RNS 1.3.x) — mirrors `RNS.vendor.umsgpack`.

    /// Build a msgpack `{"get": <command>}` request the way RNS does.
    fn msgpack_get_request(command: &str) -> Vec<u8> {
        let v = rmpv::Value::Map(vec![(rmpv::Value::from("get"), rmpv::Value::from(command))]);
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, &v).unwrap();
        buf
    }

    #[test]
    fn test_detect_codec_pickle_vs_msgpack() {
        // Pickle protocol-2 stream begins with the 0x80 PROTO opcode.
        assert_eq!(
            detect_codec(&build_get_request("interface_stats")),
            Codec::Pickle
        );
        // msgpack fixmap-with-1-entry begins with 0x81.
        let mp = msgpack_get_request("interface_stats");
        assert_eq!(mp[0], 0x81);
        assert_eq!(detect_codec(&mp), Codec::Msgpack);
    }

    #[test]
    fn test_parse_msgpack_get_request() {
        let data = msgpack_get_request("interface_stats");
        let (req, codec) = parse_request(&data).unwrap();
        assert_eq!(codec, Codec::Msgpack);
        assert!(matches!(req, RpcRequest::GetInterfaceStats));
    }

    #[test]
    fn test_parse_msgpack_path_table_with_max_hops() {
        // Mirrors RNS 1.3.x: {"get": "path_table", "max_hops": 5}
        let v = rmpv::Value::Map(vec![
            (rmpv::Value::from("get"), rmpv::Value::from("path_table")),
            (rmpv::Value::from("max_hops"), rmpv::Value::from(5i64)),
        ]);
        let mut data = Vec::new();
        rmpv::encode::write_value(&mut data, &v).unwrap();
        let (req, codec) = parse_request(&data).unwrap();
        assert_eq!(codec, Codec::Msgpack);
        assert!(matches!(
            req,
            RpcRequest::GetPathTable { max_hops: Some(5) }
        ));
    }

    #[test]
    fn test_parse_msgpack_bin_field_maps_to_bytes() {
        // destination_hash arrives as a msgpack bin -> Value::Bytes
        let hash = vec![0xABu8; 16];
        let v = rmpv::Value::Map(vec![
            (rmpv::Value::from("get"), rmpv::Value::from("next_hop")),
            (
                rmpv::Value::from("destination_hash"),
                rmpv::Value::Binary(hash.clone()),
            ),
        ]);
        let mut data = Vec::new();
        rmpv::encode::write_value(&mut data, &v).unwrap();
        let (req, _) = parse_request(&data).unwrap();
        match req {
            RpcRequest::GetNextHop { destination_hash } => assert_eq!(destination_hash, hash),
            other => panic!("expected GetNextHop, got {:?}", other),
        }
    }

    #[test]
    fn test_serialize_response_msgpack_round_trip() {
        // str must encode as msgpack `str` and bytes as msgpack `bin` so that
        // umsgpack decodes them back to Python str/bytes respectively.
        let response = pickle_dict(vec![
            (pickle_str_key("transport_id"), pickle_bytes(&[0x42; 16])),
            (pickle_str_key("transport_uptime"), pickle_float(123.456)),
            (pickle_str_key("name"), pickle_str("iface0")),
            (pickle_str_key("interfaces"), pickle_list(vec![])),
        ]);
        let bytes = serialize_response(&response, Codec::Msgpack).unwrap();

        let decoded = rmpv::decode::read_value(&mut &bytes[..]).unwrap();
        let map = match decoded {
            rmpv::Value::Map(m) => m,
            other => panic!("expected map, got {:?}", other),
        };
        let get = |k: &str| {
            map.iter()
                .find(|(key, _)| key.as_str() == Some(k))
                .map(|(_, v)| v)
        };
        assert!(
            matches!(get("transport_id"), Some(rmpv::Value::Binary(_))),
            "bytes must encode as msgpack bin"
        );
        assert!(
            matches!(get("name"), Some(rmpv::Value::String(_))),
            "str must encode as msgpack str"
        );
        assert!(matches!(get("interfaces"), Some(rmpv::Value::Array(_))));
    }

    // identity_data lifecycle (rnid retain). Mirrors destination_data parsing,
    // reading the `identity_hash` bytes field. Before this arm existed the
    // request fell through to InvalidFormat("unrecognized request"), which
    // dropped the local client connection.

    #[test]
    fn test_parse_identity_data_retain_pickle() {
        let hash = vec![0x5Au8; 16];
        let dict = pickle_dict(vec![
            (pickle_str_key("identity_data"), pickle_str("retain")),
            (pickle_str_key("identity_hash"), pickle_bytes(&hash)),
        ]);
        let data = serde_pickle::value_to_vec(&dict, Default::default()).unwrap();
        let (req, codec) = parse_request(&data).unwrap();
        assert_eq!(codec, Codec::Pickle);
        match req {
            RpcRequest::IdentityDataRetain { identity_hash } => assert_eq!(identity_hash, hash),
            other => panic!("expected IdentityDataRetain, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_identity_data_retain_msgpack() {
        // Exactly what rnid sends: mp.packb({"identity_data": "retain",
        // "identity_hash": <16 bytes>}) with the hash as a msgpack bin.
        let hash = vec![0x5Au8; 16];
        let v = rmpv::Value::Map(vec![
            (
                rmpv::Value::from("identity_data"),
                rmpv::Value::from("retain"),
            ),
            (
                rmpv::Value::from("identity_hash"),
                rmpv::Value::Binary(hash.clone()),
            ),
        ]);
        let mut data = Vec::new();
        rmpv::encode::write_value(&mut data, &v).unwrap();
        let (req, codec) = parse_request(&data).unwrap();
        assert_eq!(codec, Codec::Msgpack);
        match req {
            RpcRequest::IdentityDataRetain { identity_hash } => assert_eq!(identity_hash, hash),
            other => panic!("expected IdentityDataRetain, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_identity_data_unretain_pickle() {
        let hash = vec![0x77u8; 16];
        let dict = pickle_dict(vec![
            (pickle_str_key("identity_data"), pickle_str("unretain")),
            (pickle_str_key("identity_hash"), pickle_bytes(&hash)),
        ]);
        let data = serde_pickle::value_to_vec(&dict, Default::default()).unwrap();
        let (req, _) = parse_request(&data).unwrap();
        match req {
            RpcRequest::IdentityDataUnretain { identity_hash } => assert_eq!(identity_hash, hash),
            other => panic!("expected IdentityDataUnretain, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_identity_data_missing_hash_errors() {
        // identity_data without identity_hash is a clean parse error, never a panic.
        let dict = pickle_dict(vec![(
            pickle_str_key("identity_data"),
            pickle_str("retain"),
        )]);
        let data = serde_pickle::value_to_vec(&dict, Default::default()).unwrap();
        assert!(parse_request(&data).is_err());
    }

    #[test]
    fn test_parse_identity_data_unknown_op_errors() {
        let hash = vec![0x01u8; 16];
        let dict = pickle_dict(vec![
            (pickle_str_key("identity_data"), pickle_str("frobnicate")),
            (pickle_str_key("identity_hash"), pickle_bytes(&hash)),
        ]);
        let data = serde_pickle::value_to_vec(&dict, Default::default()).unwrap();
        assert!(parse_request(&data).is_err());
    }
}
