//! Remote transport-instance status flow — `rnstatus -R` client (Codeberg #86).
//!
//! Mirrors Python `rnstatus -R` (`reference/Reticulum/RNS/Utilities/rnstatus.py`,
//! `get_remote_status`): request a path to a remote transport instance's
//! `rnstransport.remote.management` destination, recall its identity, establish
//! an outbound link, identify with the management identity, and issue a
//! `/status` request over the link. The returned stats bundle is the same
//! `get_interface_stats()` dict a local query returns, so the same renderer
//! draws remote and local output identically.
//!
//! The server side is a transport instance (Python `rnsd` or `lnsd`) with
//! remote management enabled and the management identity's hash in its
//! allow-list (`reference/Reticulum/RNS/Transport.py:253-259`,
//! `remote_status_handler`). The wire protocol is the single-packet
//! request/response subprotocol proven against Python in
//! `tests/rnsd_interop/request_tests.rs`.
//!
//! This lives in `leviculum-std` (not the CLI) so the `lnstatus` binary and the
//! `rnsd_interop` tests exercise one implementation: the protocol flow is
//! media-agnostic and belongs beside the other link primitives.

use std::time::{Duration, Instant};

use crate::driver::{EventReceiver, ReticulumNode};
use crate::{Destination, DestinationHash, Identity, NodeEvent};

/// Transport app name for the remote management destination
/// (`RNS.Transport.APP_NAME`, `Transport.py:61`).
const REMOTE_APP_NAME: &str = "rnstransport";

/// Compute the `rnstransport.remote.management` destination hash from a remote
/// transport identity hash.
///
/// Mirrors Python `RNS.Destination.hash_from_name_and_identity(
/// "rnstransport.remote.management", identity_hash)` (`Destination.py:141`):
/// `truncated_hash(name_hash || identity_hash)` where
/// `name_hash = full_hash("rnstransport.remote.management")[:10]`.
pub fn mgmt_destination_hash(identity_hash: &[u8; 16]) -> DestinationHash {
    let name_hash = Destination::compute_name_hash(REMOTE_APP_NAME, &["remote", "management"]);
    Destination::compute_destination_hash(&name_hash, identity_hash)
}

/// Encode the `/status` request payload: msgpack `[include_lstats]` — a
/// one-element array holding a single bool, matching Python
/// `link.request("/status", data=[include_lstats], ...)`.
fn encode_status_request(include_lstats: bool) -> Vec<u8> {
    // fixarray(1) header (0x9_ | len), then a msgpack bool (0xc3 true / 0xc2 false).
    vec![0x91, if include_lstats { 0xc3 } else { 0xc2 }]
}

/// Lowercase hex, matching the RPC path's `bin` → hex decoding so the renderer
/// treats remote and local status identically.
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn f64_to_json(f: f64) -> serde_json::Value {
    serde_json::Number::from_f64(f)
        .map(serde_json::Value::Number)
        .unwrap_or(serde_json::Value::Null)
}

/// Convert a decoded msgpack value into the same `serde_json::Value` shape the
/// RPC path produces (`crate::rpc::pickle_value_to_json`): `bin` values become
/// lowercase hex strings, maps become objects with stringified keys. This lets
/// the status renderer consume a remote response byte-identically to a local
/// one.
fn rmpv_to_json(v: &rmpv::Value) -> serde_json::Value {
    use rmpv::Value as R;
    use serde_json::Value as J;
    match v {
        R::Nil => J::Null,
        R::Boolean(b) => J::Bool(*b),
        R::Integer(n) => {
            if let Some(i) = n.as_i64() {
                J::from(i)
            } else if let Some(u) = n.as_u64() {
                J::from(u)
            } else {
                J::String(n.to_string())
            }
        }
        R::F32(f) => f64_to_json(*f as f64),
        R::F64(f) => f64_to_json(*f),
        R::String(s) => match s.as_str() {
            Some(st) => J::String(st.to_string()),
            // Non-UTF8 msgpack str: fall back to hex like a bin value.
            None => J::String(hex_lower(s.as_bytes())),
        },
        R::Binary(b) => J::String(hex_lower(b)),
        R::Array(items) => J::Array(items.iter().map(rmpv_to_json).collect()),
        R::Map(pairs) => J::Object(
            pairs
                .iter()
                .map(|(k, val)| (rmpv_key_string(k), rmpv_to_json(val)))
                .collect(),
        ),
        R::Ext(_, data) => J::String(hex_lower(data)),
    }
}

/// Stringify a msgpack map key the same way the RPC path stringifies pickle
/// dict keys (`pickle_hashable_key_string`).
fn rmpv_key_string(k: &rmpv::Value) -> String {
    use rmpv::Value as R;
    match k {
        R::String(s) => s
            .as_str()
            .map(|st| st.to_string())
            .unwrap_or_else(|| hex_lower(s.as_bytes())),
        R::Binary(b) => hex_lower(b),
        R::Integer(n) => n.to_string(),
        R::Boolean(b) => b.to_string(),
        R::F64(f) => f.to_string(),
        R::F32(f) => f.to_string(),
        R::Nil => "null".to_string(),
        other => format!("{other:?}"),
    }
}

/// Extract `(stats, link_count)` from a decoded response value, which is the
/// list `[stats_dict, link_count?]` the Python handler returns (`[get_interface_stats()]`
/// or `[get_interface_stats(), get_link_count()]`).
fn extract_response(response: &rmpv::Value) -> Result<(serde_json::Value, Option<i64>), String> {
    let arr = response
        .as_array()
        .ok_or_else(|| "remote status response was not an array".to_string())?;
    if arr.is_empty() {
        return Err("remote status response was empty".to_string());
    }
    let stats = rmpv_to_json(&arr[0]);
    let link_count = arr.get(1).and_then(|v| match v {
        rmpv::Value::Integer(n) => n.as_i64().or_else(|| n.as_u64().map(|u| u as i64)),
        _ => None,
    });
    Ok((stats, link_count))
}

/// Decode a single-packet `/status` response payload (Python
/// `RNS.Packet(..., context=RESPONSE)` when the packed response fits the link
/// MDU). The payload is one msgpack value: the response list itself.
pub fn decode_status_response(
    response_data: &[u8],
) -> Result<(serde_json::Value, Option<i64>), String> {
    let mut cursor = std::io::Cursor::new(response_data);
    let value = rmpv::decode::read_value(&mut cursor)
        .map_err(|e| format!("could not decode remote status response: {e}"))?;
    extract_response(&value)
}

/// Decode a resource-carried `/status` response (Python `RNS.Resource(
/// umsgpack.packb([request_id, response]), is_response=True)` when the packed
/// response exceeds the link MDU — the usual case for a real interface-stats
/// bundle). The assembled resource data is one msgpack value: `[request_id,
/// response]`; the response list is its second element.
pub fn decode_status_resource(data: &[u8]) -> Result<(serde_json::Value, Option<i64>), String> {
    let mut cursor = std::io::Cursor::new(data);
    let value = rmpv::decode::read_value(&mut cursor)
        .map_err(|e| format!("could not decode remote status resource: {e}"))?;
    let outer = value
        .as_array()
        .ok_or_else(|| "remote status resource was not an array".to_string())?;
    let response = outer
        .get(1)
        .ok_or_else(|| "remote status resource missing response payload".to_string())?;
    extract_response(response)
}

/// Run the full `rnstatus -R` flow against a remote transport instance and
/// return its status bundle (stats dict + optional link count).
///
/// `identity_hash` is the remote transport instance's identity hash (the `-R`
/// argument). `identity` is the local management identity (`-i`) proven to the
/// remote via `link.identify` so it passes the `remote_management_allowed`
/// allow-list. `include_lstats` maps to `-l` (request the link count too).
///
/// `node` must already be started (its transport provides reachability to the
/// remote) and `events` must be its event receiver.
pub async fn fetch_remote_status(
    node: &ReticulumNode,
    events: &mut EventReceiver,
    identity_hash: &[u8; 16],
    identity: &Identity,
    include_lstats: bool,
    timeout: Duration,
    quiet: bool,
) -> Result<(serde_json::Value, Option<i64>), String> {
    let dest_hash = mgmt_destination_hash(identity_hash);

    // 1. Ensure a path to the remote management destination. Issue the initial
    // request immediately (the leaf is explicitly asking), then let
    // `wait_for_path` re-request on a bounded cadence so a delayed answer — e.g.
    // an upstream Python `rnsd` holding a forwarded announce under ingress
    // limiting (Codeberg #44) — still resolves within the timeout instead of
    // failing outright.
    if !node.has_path(&dest_hash) {
        if !quiet {
            eprintln!("Path to {} requested", hex_lower(identity_hash));
        }
        node.request_path(&dest_hash)
            .await
            .map_err(|e| e.to_string())?;
        let retry_interval = Duration::from_secs(2).min(timeout);
        if !node
            .wait_for_path(&dest_hash, timeout, retry_interval)
            .await
            .map_err(|e| e.to_string())?
        {
            return Err("Path request timed out".to_string());
        }
    }

    // 2. Recall the remote identity and extract its Ed25519 signing key.
    let remote_identity = node.get_identity(&dest_hash).ok_or_else(|| {
        "Could not recall remote identity (no announce received for the management destination)"
            .to_string()
    })?;
    let pk = remote_identity.public_key_bytes();
    let mut signing_key = [0u8; 32];
    signing_key.copy_from_slice(&pk[32..64]);

    // 3. Establish an outbound link to the management destination.
    if !quiet {
        eprintln!("Establishing link with remote transport instance...");
    }
    let _handle = node
        .connect(&dest_hash, &signing_key)
        .await
        .map_err(|e| e.to_string())?;
    let link_id = loop {
        match events.recv().await {
            Some(NodeEvent::LinkEstablished { link_id, .. }) => break link_id,
            Some(NodeEvent::LinkClosed { .. }) => {
                return Err("The link was closed before it could be established".to_string());
            }
            None => return Err("Event channel closed".to_string()),
            _ => {}
        }
    };

    // 4. Identify with the management identity (allow-list authentication).
    node.identify_link(&link_id, identity)
        .await
        .map_err(|e| format!("could not identify to remote: {e}"))?;

    // A real interface-stats bundle exceeds the link MDU (~431 B), so Python
    // returns it as a response Resource rather than a single RESPONSE packet
    // (`RNS/Link.py:900-903`). Accept incoming resources on this link before
    // sending the request so the transfer the server starts is not rejected.
    node.set_resource_strategy(
        &link_id,
        leviculum_core::resource::ResourceStrategy::AcceptAll,
    )
    .map_err(|e| e.to_string())?;

    // 5. Issue the `/status` request over the link.
    if !quiet {
        eprintln!("Sending request...");
    }
    let req_data = encode_status_request(include_lstats);
    let request_id = node
        .send_request(
            &link_id,
            "/status",
            Some(&req_data),
            Some(timeout.as_millis() as u64),
        )
        .await
        .map_err(|e| e.to_string())?;

    // 6. Await the response. A small response arrives as a single RESPONSE
    // packet (`ResponseReceived`); a large one arrives as an accepted resource
    // (`ResourceCompleted`), possibly in multiple segments.
    let deadline = Instant::now() + timeout + Duration::from_secs(2);
    let mut resource_buf: Vec<u8> = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("The remote status request timed out".to_string());
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(NodeEvent::ResponseReceived {
                request_id: rid,
                response_data,
                ..
            })) if rid == request_id => {
                return decode_status_response(&response_data);
            }
            Ok(Some(NodeEvent::ResourceCompleted {
                link_id: rlink,
                data,
                is_sender: false,
                segment_index,
                total_segments,
                ..
            })) if rlink == link_id => {
                resource_buf.extend_from_slice(&data);
                if segment_index >= total_segments {
                    return decode_status_resource(&resource_buf);
                }
            }
            Ok(Some(NodeEvent::ResourceFailed { link_id: rlink, .. })) if rlink == link_id => {
                return Err("The remote status transfer failed".to_string());
            }
            Ok(Some(NodeEvent::RequestTimedOut {
                request_id: rid, ..
            })) if rid == request_id => {
                return Err(
                    "The remote status request failed. Likely authentication failure.".to_string(),
                );
            }
            Ok(Some(NodeEvent::LinkClosed { .. })) => {
                return Err("The link was closed by the server, exiting now".to_string());
            }
            Ok(Some(_)) => continue,
            Ok(None) => return Err("Event channel closed".to_string()),
            Err(_) => return Err("The remote status request timed out".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mgmt_destination_hash_matches_python_derivation() {
        // Python: name_hash = full_hash("rnstransport.remote.management")[:10];
        // dest = truncated_hash(name_hash + identity_hash). Reproduce the
        // reference computation independently here from first principles.
        use sha2::{Digest, Sha256};
        let identity_hash = [0xABu8; 16];

        let name_hash = &Sha256::digest(b"rnstransport.remote.management")[..10];
        let mut combined = Vec::new();
        combined.extend_from_slice(name_hash);
        combined.extend_from_slice(&identity_hash);
        let expected = &Sha256::digest(&combined)[..16];

        let got = mgmt_destination_hash(&identity_hash);
        assert_eq!(got.as_bytes(), expected);
    }

    #[test]
    fn encode_status_request_is_one_element_array() {
        assert_eq!(encode_status_request(true), vec![0x91, 0xc3]);
        assert_eq!(encode_status_request(false), vec![0x91, 0xc2]);
    }

    #[test]
    fn decode_status_response_stats_and_link_count() {
        // Build msgpack for [ {"interfaces": [ {"name": "eth0", "rxb": 10} ]}, 3 ].
        use rmpv::Value as V;
        let ifstat = V::Map(vec![
            (V::from("name"), V::from("eth0")),
            (V::from("rxb"), V::from(10u64)),
        ]);
        let stats_dict = V::Map(vec![(V::from("interfaces"), V::Array(vec![ifstat]))]);
        let response = V::Array(vec![stats_dict, V::from(3u64)]);
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, &response).unwrap();

        let (stats, link_count) = decode_status_response(&buf).unwrap();
        assert_eq!(link_count, Some(3));
        let ifaces = stats.get("interfaces").and_then(|v| v.as_array()).unwrap();
        assert_eq!(ifaces.len(), 1);
        assert_eq!(ifaces[0].get("name").and_then(|v| v.as_str()), Some("eth0"));
        assert_eq!(ifaces[0].get("rxb").and_then(|v| v.as_i64()), Some(10));
    }

    #[test]
    fn decode_status_resource_unwraps_request_id() {
        // Large responses arrive as a resource carrying msgpack
        // [request_id, [stats_dict, link_count]].
        use rmpv::Value as V;
        let stats_dict = V::Map(vec![(
            V::from("interfaces"),
            V::Array(vec![V::Map(vec![(V::from("name"), V::from("eth0"))])]),
        )]);
        let response = V::Array(vec![stats_dict, V::from(7u64)]);
        let request_id = V::Binary(vec![0x11; 16]);
        let outer = V::Array(vec![request_id, response]);
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, &outer).unwrap();

        let (stats, link_count) = decode_status_resource(&buf).unwrap();
        assert_eq!(link_count, Some(7));
        assert_eq!(
            stats
                .get("interfaces")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(1)
        );
    }

    #[test]
    fn decode_status_response_bin_becomes_hex() {
        // A `bin` value (e.g. parent_interface_hash) must render as lowercase hex.
        use rmpv::Value as V;
        let stats_dict = V::Map(vec![(
            V::from("switch_id"),
            V::Binary(vec![0xde, 0xad, 0xbe, 0xef]),
        )]);
        let response = V::Array(vec![stats_dict]);
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, &response).unwrap();

        let (stats, link_count) = decode_status_response(&buf).unwrap();
        assert_eq!(link_count, None);
        assert_eq!(
            stats.get("switch_id").and_then(|v| v.as_str()),
            Some("deadbeef")
        );
    }
}
