//! RPC server for Python CLI tool compatibility
//!
//! Implements the `multiprocessing.connection` wire protocol so that Python
//! tools (`rnstatus`, `rnpath`, `rnprobe`) can query the running Rust daemon
//! as if it were a Python shared instance.
//!
//! Three layers:
//! - `connection`: Wire protocol (length-prefixed framing, HMAC handshake)
//! - `pickle`: Request parsing and response building (pickle ser/de)
//! - `handlers`: RPC command dispatch and state queries

pub(crate) mod connection;
mod error;
pub(crate) mod handlers;
pub(crate) mod pickle;

use std::sync::{Arc, Mutex};

use crate::sync_ext::MutexRecover;

// RPC transport. Python `multiprocessing.connection` runs over Unix sockets on
// Unix and over TCP loopback (AF_INET, default local_control_port 37429) on
// Windows; we mirror that so `rnstatus`/`rnpath` interop on each platform. The
// framing + HMAC auth (connection.rs) are transport-agnostic.
//
// Platform support: Linux (abstract Unix sockets) is the tested path, exercised
// by our CI. The macOS/BSD filesystem-socket and Windows TCP-loopback fallbacks
// are community-supported and are not exercised by our CI.
#[cfg(windows)]
use tokio::net::TcpListener as RpcListener;
#[cfg(windows)]
use tokio::net::TcpStream as RpcStream;
#[cfg(unix)]
use tokio::net::UnixListener as RpcListener;
#[cfg(unix)]
use tokio::net::UnixStream as RpcStream;

use crate::driver::{AutoPeerCount, StdNodeCore};
use crate::interfaces::inventory::SharedInventory;
use crate::interfaces::InterfaceStatsMap;
use connection::{read_message, server_handshake, write_message};
use error::RpcError;
use handlers::handle_request;
use pickle::parse_request;

/// Bind the RPC listener for the given abstract name.
///
/// On Linux, uses abstract Unix sockets; on other Unix systems, filesystem
/// sockets in the temp directory.
#[cfg(unix)]
fn bind_rpc_listener(
    abstract_name: &str,
) -> Result<std::os::unix::net::UnixListener, std::io::Error> {
    #[cfg(target_os = "linux")]
    {
        use std::os::linux::net::SocketAddrExt;
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(abstract_name.as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        std::os::unix::net::UnixListener::bind_addr(&addr)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let path =
            std::env::temp_dir().join(format!("leviculum-{}", abstract_name.replace('/', "-")));
        let _ = std::fs::remove_file(&path);
        std::os::unix::net::UnixListener::bind(&path)
    }
}

/// Windows: bind the RPC listener on TCP loopback (Python-RNS AF_INET fallback).
#[cfg(windows)]
fn bind_rpc_listener(abstract_name: &str) -> Result<std::net::TcpListener, std::io::Error> {
    std::net::TcpListener::bind(crate::interfaces::local::loopback_addr(abstract_name))
}

/// Connect to an RPC socket by abstract name.
#[cfg(unix)]
fn connect_rpc(abstract_name: &str) -> Result<std::os::unix::net::UnixStream, std::io::Error> {
    #[cfg(target_os = "linux")]
    {
        use std::os::linux::net::SocketAddrExt;
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(abstract_name.as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        std::os::unix::net::UnixStream::connect_addr(&addr)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let path =
            std::env::temp_dir().join(format!("leviculum-{}", abstract_name.replace('/', "-")));
        std::os::unix::net::UnixStream::connect(&path)
    }
}

/// Windows: connect to the TCP loopback RPC socket.
#[cfg(windows)]
fn connect_rpc(abstract_name: &str) -> Result<std::net::TcpStream, std::io::Error> {
    std::net::TcpStream::connect(crate::interfaces::local::loopback_addr(abstract_name))
}

/// Spawn the RPC server on abstract Unix socket `\0rns/{instance_name}/rpc`.
///
/// Accepts connections concurrently (each in its own task).
/// Each connection: handshake -> read request -> dispatch -> write response -> close.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_rpc_server(
    instance_name: &str,
    core: Arc<Mutex<StdNodeCore>>,
    authkey: [u8; 32],
    start_time: std::time::Instant,
    iface_stats_map: InterfaceStatsMap,
    inventory: SharedInventory,
    auto_peer_count: AutoPeerCount,
    discovery_storage: Option<std::path::PathBuf>,
) -> Result<(), std::io::Error> {
    let abstract_name = format!("rns/{}/rpc", instance_name);

    let std_listener = bind_rpc_listener(&abstract_name)?;
    std_listener.set_nonblocking(true)?;
    let listener = RpcListener::from_std(std_listener)?;

    tracing::info!("RPC server listening on socket {}", abstract_name);

    tokio::spawn(async move {
        rpc_accept_loop(
            listener,
            core,
            authkey,
            start_time,
            iface_stats_map,
            inventory,
            auto_peer_count,
            discovery_storage,
        )
        .await;
    });

    Ok(())
}

/// Accept loop: spawns a task per connection.
#[allow(clippy::too_many_arguments)]
async fn rpc_accept_loop(
    listener: RpcListener,
    core: Arc<Mutex<StdNodeCore>>,
    authkey: [u8; 32],
    start_time: std::time::Instant,
    iface_stats_map: InterfaceStatsMap,
    inventory: SharedInventory,
    auto_peer_count: AutoPeerCount,
    discovery_storage: Option<std::path::PathBuf>,
) {
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!("RPC accept error: {}", e);
                continue;
            }
        };

        let core = Arc::clone(&core);
        let stats_map = Arc::clone(&iface_stats_map);
        let inventory = Arc::clone(&inventory);
        let peer_count = auto_peer_count.clone();
        let discovery_storage = discovery_storage.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_rpc_connection(
                stream,
                &core,
                &authkey,
                start_time,
                &stats_map,
                &inventory,
                &peer_count,
                discovery_storage.as_deref(),
            )
            .await
            {
                tracing::debug!("RPC connection error: {}", e);
            }
        });
    }
}

/// Handle a single RPC connection: handshake -> read -> dispatch -> write -> close.
#[allow(clippy::too_many_arguments)]
async fn handle_rpc_connection(
    mut stream: RpcStream,
    core: &Arc<Mutex<StdNodeCore>>,
    authkey: &[u8; 32],
    start_time: std::time::Instant,
    iface_stats_map: &InterfaceStatsMap,
    inventory: &SharedInventory,
    auto_peer_count: &AutoPeerCount,
    discovery_storage: Option<&std::path::Path>,
) -> Result<(), RpcError> {
    server_handshake(&mut stream, authkey).await?;

    let request_bytes = read_message(&mut stream).await?;
    let (request, codec) = parse_request(&request_bytes)?;

    tracing::debug!("RPC request: {:?} ({:?})", request, codec);

    let response_bytes = {
        let mut core = core.lock_recover();
        let peer_count = auto_peer_count.total();
        handle_request(
            &request,
            &mut core,
            start_time,
            iface_stats_map,
            inventory,
            peer_count,
            discovery_storage,
            codec,
        )?
    };

    write_message(&mut stream, &response_bytes).await?;

    Ok(())
}

// Client-side functions

/// Wall-clock cap on the shared-instance RPC handshake + request/response.
///
/// The socket is a local abstract Unix socket, so 5 s is generous. Its real
/// job is to keep a client (`lnstest diag`, `rnstatus`, …)
/// from blocking forever when the daemon is unresponsive or speaks a slightly
/// different RPC dialect — e.g. a Python `rnsd` that hits an error handling an
/// unexpected request and sends no response at all.
const RPC_CLIENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Connect to the RPC server, perform handshake, send request, receive response.
///
/// The whole handshake + round-trip is bounded by [`RPC_CLIENT_TIMEOUT`]; on
/// expiry this returns a `TimedOut` I/O error rather than hanging.
pub(crate) async fn rpc_client_call(
    abstract_name: &str,
    authkey: &[u8; 32],
    request: &serde_pickle::value::Value,
) -> Result<serde_pickle::value::Value, RpcError> {
    // `connect_rpc` either succeeds quickly or fails fast (ECONNREFUSED /
    // connection refused if nothing is listening) on the loopback transports we
    // use, so it stays outside the request/response timeout below.
    let std_stream = connect_rpc(abstract_name)?;
    std_stream.set_nonblocking(true)?;
    let mut stream = RpcStream::from_std(std_stream)?;

    // Upstream Python-RNS migrated the shared-instance RPC payload from pickle to
    // msgpack (reference/Reticulum a2ef9782): the client now sends `mp.packb(req)`
    // and reads `mp.unpackb(resp)`. We match that on the wire. The framing
    // (4-byte length prefix) and the HMAC handshake are unchanged. Our own server
    // still auto-detects pickle vs msgpack, so this stays compatible with both
    // current upstream `rnsd` and our `lnsd`.
    let request_bytes = pickle::encode_request_msgpack(request)?;

    let exchange = async {
        connection::client_handshake(&mut stream, authkey).await?;
        write_message(&mut stream, &request_bytes).await?;
        let response_bytes = read_message(&mut stream).await?;
        let response = pickle::decode_response_msgpack(&response_bytes)?;
        Ok::<_, RpcError>(response)
    };

    match tokio::time::timeout(RPC_CLIENT_TIMEOUT, exchange).await {
        Ok(result) => result,
        Err(_elapsed) => Err(RpcError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "shared-instance RPC did not complete within {} s",
                RPC_CLIENT_TIMEOUT.as_secs()
            ),
        ))),
    }
}

/// Issue a parameterless `get` query against a running shared-instance daemon's
/// RPC socket (`\0rns/{instance_name}/rpc`) and return the response decoded into
/// a [`serde_json::Value`].
///
/// `get_key` must be one of the parameterless RPC keys understood by the daemon:
/// `"interface_stats"`, `"path_table"`, `"link_count"`, `"active_link_count"`,
/// `"lowest_interface_bitrate"`, `"medium_path_timeout"`, `"link_table"`,
/// `"rate_table"`, `"blackholed_identities"`, `"transport_tables"`.
/// All but `"link_table"` and `"transport_tables"` overlap with Python `rnsd`
/// (the three timing/link verbs postdate the vendored 1.3.5 reference and were
/// taken from 1.5.2); those two are Leviculum-only extensions (Codeberg #174
/// for the latter) and degrade to an `Err` against an `rnsd`, which matches no arm
/// for them and closes the connection without replying
/// (Reticulum.py:1213-1260). Callers must treat that error as "this daemon
/// does not know the question", not as a failed query. Queries that take
/// parameters (`next_hop`, `packet_rssi`, …) and the mutating
/// `drop`/`blackhole`/`destination_data` ops are intentionally not reachable
/// through this helper.
///
/// `authkey` is `SHA256(transport_identity)` — the daemon derives the same key
/// from its `{config_dir}/storage/transport_identity` file (raw 64 bytes).
///
/// Returns the response as JSON: msgpack maps become objects (non-string keys
/// stringified), `bin` values become lowercase hex strings, arrays become
/// arrays, `nil` becomes `null`, big ints become decimal strings. (The wire
/// codec is msgpack; the decoded value tree is the same `serde_pickle::Value`
/// model the printing code consumes, so the conversion is unchanged.)
pub async fn rpc_query(
    instance_name: &str,
    authkey: &[u8; 32],
    get_key: &str,
) -> Result<serde_json::Value, crate::Error> {
    let abstract_name = format!("rns/{}/rpc", instance_name);
    let mut entries = vec![(pickle::pickle_str_key("get"), pickle::pickle_str(get_key))];
    // Python `rnsd`'s `path_table` RPC handler reads `call["max_hops"]`
    // unconditionally (it `KeyError`s — and then sends no response — when the
    // key is absent). Python's own client always sends the key, value `None`
    // for "no hop limit" (RNS/Reticulum.py:1331). Our server treats absent and
    // `None` identically, so this is harmless against `lnsd` too.
    if get_key == "path_table" {
        entries.push((
            pickle::pickle_str_key("max_hops"),
            serde_pickle::value::Value::None,
        ));
    }
    let request = pickle::pickle_dict(entries);
    let response = rpc_client_call(&abstract_name, authkey, &request)
        .await
        .map_err(|e| match e {
            RpcError::Io(io) => crate::Error::Io(io),
            other => crate::Error::Config(format!("shared-instance RPC error: {other}")),
        })?;
    Ok(pickle_value_to_json(&response))
}

/// Issue a `get` query that takes a single hash parameter (`next_hop`,
/// `next_hop_if_name`, `first_hop_timeout`, `packet_rssi`, `packet_snr`,
/// `packet_q`) against a running shared-instance daemon's RPC socket.
///
/// `param_key` names the parameter the daemon's parser reads for `get_key`
/// (`"destination_hash"` for the path verbs, `"packet_hash"` for the packet
/// verbs); `param` is the raw hash bytes, sent as msgpack `bin` exactly as
/// Python's client sends them (Reticulum.py:1331 region). Same authkey,
/// codec, and JSON decoding rules as [`rpc_query`]; same caveat that an
/// unanswered verb closes the connection and surfaces as `Err`.
pub async fn rpc_query_hash_param(
    instance_name: &str,
    authkey: &[u8; 32],
    get_key: &str,
    param_key: &str,
    param: &[u8],
) -> Result<serde_json::Value, crate::Error> {
    let abstract_name = format!("rns/{}/rpc", instance_name);
    let entries = vec![
        (pickle::pickle_str_key("get"), pickle::pickle_str(get_key)),
        (
            pickle::pickle_str_key(param_key),
            pickle::pickle_bytes(param),
        ),
    ];
    let request = pickle::pickle_dict(entries);
    let response = rpc_client_call(&abstract_name, authkey, &request)
        .await
        .map_err(|e| match e {
            RpcError::Io(io) => crate::Error::Io(io),
            other => crate::Error::Config(format!("shared-instance RPC error: {other}")),
        })?;
    Ok(pickle_value_to_json(&response))
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn f64_to_json(f: f64) -> serde_json::Value {
    serde_json::Number::from_f64(f)
        .map(serde_json::Value::Number)
        .unwrap_or(serde_json::Value::Null)
}

fn pickle_value_to_json(v: &serde_pickle::value::Value) -> serde_json::Value {
    use serde_json::Value as J;
    use serde_pickle::value::Value as P;
    match v {
        P::None => J::Null,
        P::Bool(b) => J::Bool(*b),
        P::I64(n) => J::from(*n),
        P::Int(n) => J::String(n.to_string()),
        P::F64(f) => f64_to_json(*f),
        P::Bytes(b) => J::String(hex_lower(b)),
        P::String(s) => J::String(s.clone()),
        P::List(items) | P::Tuple(items) => {
            J::Array(items.iter().map(pickle_value_to_json).collect())
        }
        P::Set(items) | P::FrozenSet(items) => {
            J::Array(items.iter().map(pickle_hashable_to_json).collect())
        }
        P::Dict(d) => J::Object(
            d.iter()
                .map(|(k, v)| (pickle_hashable_key_string(k), pickle_value_to_json(v)))
                .collect(),
        ),
    }
}

fn pickle_hashable_to_json(h: &serde_pickle::value::HashableValue) -> serde_json::Value {
    use serde_json::Value as J;
    use serde_pickle::value::HashableValue as H;
    match h {
        H::None => J::Null,
        H::Bool(b) => J::Bool(*b),
        H::I64(n) => J::from(*n),
        H::Int(n) => J::String(n.to_string()),
        H::F64(f) => f64_to_json(*f),
        H::Bytes(b) => J::String(hex_lower(b)),
        H::String(s) => J::String(s.clone()),
        H::Tuple(items) => J::Array(items.iter().map(pickle_hashable_to_json).collect()),
        H::FrozenSet(items) => J::Array(items.iter().map(pickle_hashable_to_json).collect()),
    }
}

fn pickle_hashable_key_string(h: &serde_pickle::value::HashableValue) -> String {
    use serde_pickle::value::HashableValue as H;
    match h {
        H::String(s) => s.clone(),
        H::I64(n) => n.to_string(),
        H::Int(n) => n.to_string(),
        H::Bool(b) => b.to_string(),
        H::Bytes(b) => hex_lower(b),
        H::F64(f) => f.to_string(),
        H::None => "null".to_string(),
        other => format!("{other:?}"),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::interfaces::InterfaceStatsMap;
    use pickle::{pickle_dict, pickle_str, pickle_str_key};
    use serde_pickle::value::{HashableValue, Value};

    /// Derive the RPC authkey from a NodeCore identity (same as driver).
    fn derive_authkey(core: &Arc<Mutex<StdNodeCore>>) -> [u8; 32] {
        let core_guard = core.lock_recover();
        let prv = core_guard.identity().private_key_bytes().unwrap();
        use sha2::Digest;
        let hash = sha2::Sha256::digest(prv);
        let mut key = [0u8; 32];
        key.copy_from_slice(&hash);
        key
    }

    static RPC_TEST_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// Create a minimal ReticulumNode and extract its inner Arc<Mutex<StdNodeCore>>.
    fn make_test_core(transport: bool) -> Arc<Mutex<StdNodeCore>> {
        let id = RPC_TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let storage_path = std::env::temp_dir().join(format!(
            "reticulum_rpc_test_{}_{}_{}",
            std::process::id(),
            id,
            transport
        ));
        let node = crate::driver::ReticulumNodeBuilder::new()
            .storage_path(storage_path)
            .enable_transport(transport)
            .build_sync()
            .expect("build_sync failed");
        node.inner()
    }

    fn empty_stats_map() -> InterfaceStatsMap {
        Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()))
    }

    /// The shared-instance RPC client now speaks msgpack on the wire (matching
    /// upstream Python-RNS `mp.packb`/`mp.unpackb`), not pickle: a request
    /// encodes as a msgpack map (fixmap header, never the pickle `0x80` PROTO
    /// opcode), str keys/values stay msgpack `str`, byte-string values stay
    /// msgpack `bin`, and a msgpack response decodes back into the shared
    /// `Value` tree the printing/json code consumes.
    #[test]
    fn test_client_encodes_request_as_msgpack_and_round_trips() {
        use pickle::{decode_response_msgpack, encode_request_msgpack, pickle_bytes, pickle_int};

        // {"get": "next_hop", "destination_hash": <16 bytes>}
        let hash = vec![0xABu8; 16];
        let request = pickle_dict(vec![
            (pickle_str_key("get"), pickle_str("next_hop")),
            (pickle_str_key("destination_hash"), pickle_bytes(&hash)),
        ]);
        let bytes = encode_request_msgpack(&request).unwrap();

        // msgpack fixmap-with-2-entries (0x82), never the pickle 0x80 PROTO opcode.
        assert_eq!(bytes[0], 0x82, "request must be msgpack, not pickle");

        // Decode the request bytes back with a raw msgpack reader and confirm the
        // str-vs-bin typing Python umsgpack relies on: keys and "next_hop" are
        // msgpack `str`, the hash is msgpack `bin`.
        let raw = rmpv::decode::read_value(&mut &bytes[..]).unwrap();
        let map = match raw {
            rmpv::Value::Map(m) => m,
            other => panic!("expected msgpack map, got {other:?}"),
        };
        let get = |k: &str| {
            map.iter()
                .find(|(key, _)| key.as_str() == Some(k))
                .map(|(_, v)| v)
        };
        assert!(
            matches!(get("get"), Some(rmpv::Value::String(_))),
            "str value must encode as msgpack str"
        );
        assert!(
            matches!(get("destination_hash"), Some(rmpv::Value::Binary(b)) if b == &hash),
            "byte-string value must encode as msgpack bin"
        );

        // A msgpack response (as Python rnsd sends) decodes into the same Value
        // tree, preserving bin -> Bytes and int -> I64.
        let response_value = pickle_dict(vec![
            (pickle_str_key("transport_id"), pickle_bytes(&[0x42; 16])),
            (pickle_str_key("link_count"), pickle_int(3)),
        ]);
        let response_bytes = encode_request_msgpack(&response_value).unwrap();
        let decoded = decode_response_msgpack(&response_bytes).unwrap();
        let Value::Dict(d) = &decoded else {
            panic!("expected dict response, got {decoded:?}");
        };
        assert_eq!(
            d.get(&HashableValue::String("transport_id".into())),
            Some(&Value::Bytes(vec![0x42; 16])),
            "bin must decode back to Bytes"
        );
        assert_eq!(
            d.get(&HashableValue::String("link_count".into())),
            Some(&Value::I64(3)),
            "int must decode back to I64"
        );
    }

    /// Spawn a minimal RPC server and test it with a Rust client.
    ///
    /// Registers a non-local-client interface so the `interfaces` list is
    /// non-empty, then asserts (Codeberg #67 Stage 1) that every one of the 9
    /// new interface_stats keys is present with the correct type and the
    /// documented Python-matching default (transport enabled: announce_rate
    /// target=3600/penalty=0/grace=5).
    #[tokio::test]
    async fn test_rpc_interface_stats_round_trip() {
        let core = make_test_core(true);
        // Register an interface so it appears in the stats list (not a local
        // client -> not skipped by build_interface_stats).
        {
            let mut c = core.lock().unwrap();
            c.set_interface_name(0, "tcp_client_0".to_string());
            // Registered HW_MTU must come back as the `mtu` key.
            c.set_interface_hw_mtu(0, 262_144);
        }
        let start_time = std::time::Instant::now();
        let authkey = derive_authkey(&core);

        let instance_name = format!("rpctest_{}", std::process::id());
        let abstract_name = format!("rns/{}/rpc", instance_name);

        spawn_rpc_server(
            &instance_name,
            Arc::clone(&core),
            authkey,
            start_time,
            empty_stats_map(),
            crate::interfaces::inventory::InterfaceInventory::shared(),
            AutoPeerCount::default(),
            None,
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let request = pickle_dict(vec![(pickle_str_key("get"), pickle_str("interface_stats"))]);
        let response = rpc_client_call(&abstract_name, &authkey, &request)
            .await
            .unwrap();

        match &response {
            Value::Dict(d) => {
                assert!(
                    d.contains_key(&HashableValue::String("transport_id".into())),
                    "response should contain transport_id"
                );
                assert!(
                    d.contains_key(&HashableValue::String("transport_uptime".into())),
                    "response should contain transport_uptime"
                );
                assert!(
                    d.contains_key(&HashableValue::String("interfaces".into())),
                    "response should contain interfaces"
                );

                if let Some(Value::F64(uptime)) =
                    d.get(&HashableValue::String("transport_uptime".into()))
                {
                    assert!(*uptime >= 0.0, "uptime should be non-negative");
                }

                // Codeberg #67 Stage 1: the 9 new keys on the interface dict.
                let ifaces = match d.get(&HashableValue::String("interfaces".into())) {
                    Some(Value::List(l)) => l,
                    other => panic!("interfaces should be a list, got {other:?}"),
                };
                assert_eq!(ifaces.len(), 1, "exactly the one registered interface");
                let iface = match &ifaces[0] {
                    Value::Dict(m) => m,
                    other => panic!("interface entry should be a dict, got {other:?}"),
                };
                let get = |k: &str| iface.get(&HashableValue::String(k.into()));

                // Path-request frequencies: float 0.0 default.
                assert_eq!(
                    get("incoming_pr_frequency"),
                    Some(&Value::F64(0.0)),
                    "incoming_pr_frequency default 0.0"
                );
                assert_eq!(
                    get("outgoing_pr_frequency"),
                    Some(&Value::F64(0.0)),
                    "outgoing_pr_frequency default 0.0"
                );
                // Announce-rate defaults for a transport-enabled node.
                assert_eq!(
                    get("announce_rate_target"),
                    Some(&Value::I64(3600)),
                    "announce_rate_target default 3600"
                );
                assert_eq!(
                    get("announce_rate_penalty"),
                    Some(&Value::I64(0)),
                    "announce_rate_penalty default 0"
                );
                assert_eq!(
                    get("announce_rate_grace"),
                    Some(&Value::I64(5)),
                    "announce_rate_grace default 5"
                );
                // Burst flags: False/0.
                assert_eq!(
                    get("burst_active"),
                    Some(&Value::Bool(false)),
                    "burst_active default false"
                );
                assert_eq!(
                    get("burst_activated"),
                    Some(&Value::I64(0)),
                    "burst_activated default 0"
                );
                assert_eq!(
                    get("pr_burst_active"),
                    Some(&Value::Bool(false)),
                    "pr_burst_active default false"
                );
                assert_eq!(
                    get("pr_burst_activated"),
                    Some(&Value::I64(0)),
                    "pr_burst_activated default 0"
                );

                // The post-1.3.5 upstream key set, measured on Reticulum
                // 1.5.2 (`get_interface_stats`; none of these keys exist in
                // the pinned 1.3.5 reference). Its rnstatus indexes txdrp,
                // mtu and txbuffered unguarded on its default path and
                // crashed on their absence — this pins presence, type, and
                // the documented value.
                assert_eq!(
                    get("mtu"),
                    Some(&Value::I64(262_144)),
                    "mtu carries the registered HW_MTU"
                );
                assert_eq!(get("txdrp"), Some(&Value::I64(0)), "txdrp default 0");
                assert_eq!(get("txdrb"), Some(&Value::I64(0)), "txdrb default 0");
                assert_eq!(
                    get("txstalled"),
                    Some(&Value::Bool(false)),
                    "txstalled default false"
                );
                assert_eq!(
                    get("txbuffered"),
                    Some(&Value::I64(0)),
                    "txbuffered default 0"
                );
                // Sortable / flag-path keys (rnstatus --sort, -a, -p).
                for key in [
                    "arxb", "atxb", "arxc", "atxc", "prxb", "ptxb", "prxc", "ptxc",
                ] {
                    assert_eq!(get(key), Some(&Value::I64(0)), "{key} default 0");
                }
                for key in ["arxs", "atxs", "prxs", "ptxs"] {
                    assert_eq!(get(key), Some(&Value::F64(0.0)), "{key} default 0.0");
                }
                for key in [
                    "burst_count",
                    "pr_burst_count",
                    "announces_to_internal",
                    "autoconnect_source",
                ] {
                    assert_eq!(get(key), Some(&Value::None), "{key} default None");
                }
                for key in [
                    "gravity",
                    "protocol_violations",
                    "ifac_violations",
                    "packet_filter_hits",
                ] {
                    assert_eq!(get(key), Some(&Value::I64(0)), "{key} default 0");
                }

                // Top-level keys of the same 1.5.2 producer: its rnstatus
                // reads these unguarded under --pps and --queues.
                let top = |k: &str| d.get(&HashableValue::String(k.into()));
                for key in [
                    "rxpps", "txpps", "rxqt", "rxqd", "rxqa", "rxqp", "rxqil", "rxqtd", "rxqdd",
                    "rxqad", "rxqpd", "rxqild",
                ] {
                    assert_eq!(top(key), Some(&Value::I64(0)), "{key} default 0");
                }
                for key in [
                    "arxf",
                    "atxf",
                    "prxf",
                    "ptxf",
                    "tqpressure",
                    "dqpressure",
                    "aqpressure",
                    "pqpressure",
                    "ilqpressure",
                ] {
                    assert_eq!(top(key), Some(&Value::F64(0.0)), "{key} default 0.0");
                }
                assert_eq!(top("txq"), Some(&Value::None), "txq default None");
            }
            other => panic!("expected dict response, got: {:?}", other),
        }
    }

    /// The announce-rate defaults track Python's transport branch: with
    /// transport DISABLED and unconfigured they are None (Reticulum.py:798-811),
    /// so rnstatus renders no `(t:.../p:.../g:...)` suffix.
    #[tokio::test]
    async fn test_rpc_interface_stats_announce_rate_none_without_transport() {
        let core = make_test_core(false);
        core.lock()
            .unwrap()
            .set_interface_name(0, "tcp_client_0".to_string());
        let start_time = std::time::Instant::now();
        let authkey = derive_authkey(&core);

        let instance_name = format!("rpctest_arnone_{}", std::process::id());
        let abstract_name = format!("rns/{}/rpc", instance_name);

        spawn_rpc_server(
            &instance_name,
            Arc::clone(&core),
            authkey,
            start_time,
            empty_stats_map(),
            crate::interfaces::inventory::InterfaceInventory::shared(),
            AutoPeerCount::default(),
            None,
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let request = pickle_dict(vec![(pickle_str_key("get"), pickle_str("interface_stats"))]);
        let response = rpc_client_call(&abstract_name, &authkey, &request)
            .await
            .unwrap();

        let Value::Dict(d) = &response else {
            panic!("expected dict response, got: {response:?}");
        };
        let ifaces = match d.get(&HashableValue::String("interfaces".into())) {
            Some(Value::List(l)) => l,
            other => panic!("interfaces should be a list, got {other:?}"),
        };
        let iface = match &ifaces[0] {
            Value::Dict(m) => m,
            other => panic!("interface entry should be a dict, got {other:?}"),
        };
        let get = |k: &str| iface.get(&HashableValue::String(k.into()));
        assert_eq!(get("announce_rate_target"), Some(&Value::None));
        assert_eq!(get("announce_rate_penalty"), Some(&Value::None));
        assert_eq!(get("announce_rate_grace"), Some(&Value::None));
    }

    /// Codeberg #67 Stage 2a: a configured `announce_rate_*` value is read from
    /// the interface and emitted verbatim, overriding the transport-enabled
    /// Python defaults. The burst fields still carry their Stage-1 (#87)
    /// defaults, and the PR frequencies are real (0.0 for an idle interface).
    #[tokio::test]
    async fn test_rpc_interface_stats_configured_announce_rate() {
        use leviculum_core::transport::AnnounceRateConfig;

        let core = make_test_core(true);
        {
            let mut c = core.lock().unwrap();
            c.set_interface_name(0, "tcp_client_0".to_string());
            c.set_announce_rate_config(
                0,
                AnnounceRateConfig {
                    target: Some(7200),
                    penalty: Some(30),
                    grace: Some(2),
                },
            );
        }
        let start_time = std::time::Instant::now();
        let authkey = derive_authkey(&core);

        let instance_name = format!("rpctest_arcfg_{}", std::process::id());
        let abstract_name = format!("rns/{}/rpc", instance_name);

        spawn_rpc_server(
            &instance_name,
            Arc::clone(&core),
            authkey,
            start_time,
            empty_stats_map(),
            crate::interfaces::inventory::InterfaceInventory::shared(),
            AutoPeerCount::default(),
            None,
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let request = pickle_dict(vec![(pickle_str_key("get"), pickle_str("interface_stats"))]);
        let response = rpc_client_call(&abstract_name, &authkey, &request)
            .await
            .unwrap();

        let Value::Dict(d) = &response else {
            panic!("expected dict response, got: {response:?}");
        };
        let ifaces = match d.get(&HashableValue::String("interfaces".into())) {
            Some(Value::List(l)) => l,
            other => panic!("interfaces should be a list, got {other:?}"),
        };
        let iface = match &ifaces[0] {
            Value::Dict(m) => m,
            other => panic!("interface entry should be a dict, got {other:?}"),
        };
        let get = |k: &str| iface.get(&HashableValue::String(k.into()));
        // Configured announce-rate values are emitted, not the 3600/0/5 defaults.
        assert_eq!(get("announce_rate_target"), Some(&Value::I64(7200)));
        assert_eq!(get("announce_rate_penalty"), Some(&Value::I64(30)));
        assert_eq!(get("announce_rate_grace"), Some(&Value::I64(2)));
        // PR frequencies are real and read 0.0 for an idle interface.
        assert_eq!(get("incoming_pr_frequency"), Some(&Value::F64(0.0)));
        assert_eq!(get("outgoing_pr_frequency"), Some(&Value::F64(0.0)));
        // Burst fields remain at their #87 defaults.
        assert_eq!(get("burst_active"), Some(&Value::Bool(false)));
        assert_eq!(get("burst_activated"), Some(&Value::I64(0)));
        assert_eq!(get("pr_burst_active"), Some(&Value::Bool(false)));
        assert_eq!(get("pr_burst_activated"), Some(&Value::I64(0)));
    }

    /// Blackhole handlers end to end over the RPC round trip (Codeberg #67).
    /// One shared `Arc<Mutex<core>>` backs every connection, so the real set
    /// persists across the calls: blackhole -> is_blackholed true + listed ->
    /// unblackhole -> is_blackholed false + empty.
    #[tokio::test]
    async fn test_rpc_blackhole_round_trip() {
        use pickle::pickle_bytes;

        let core = make_test_core(true);
        let start_time = std::time::Instant::now();
        let authkey = derive_authkey(&core);

        let instance_name = format!("rpctest_bh_{}", std::process::id());
        let abstract_name = format!("rns/{}/rpc", instance_name);

        spawn_rpc_server(
            &instance_name,
            Arc::clone(&core),
            authkey,
            start_time,
            empty_stats_map(),
            crate::interfaces::inventory::InterfaceInventory::shared(),
            AutoPeerCount::default(),
            None,
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let hash = vec![0x9Au8; 16];
        let is_blackholed = |h: &[u8]| {
            pickle_dict(vec![
                (pickle_str_key("get"), pickle_str("is_blackholed")),
                (pickle_str_key("identity_hash"), pickle_bytes(h)),
            ])
        };

        // Initially not blackholed.
        let r = rpc_client_call(&abstract_name, &authkey, &is_blackholed(&hash))
            .await
            .unwrap();
        assert_eq!(r, Value::Bool(false), "not blackholed before insert");

        // Blackhole it: fresh insert returns bool true.
        let req = pickle_dict(vec![
            (pickle_str_key("blackhole_identity"), pickle_bytes(&hash)),
            (pickle_str_key("until"), Value::None),
            (pickle_str_key("reason"), Value::None),
        ]);
        let r = rpc_client_call(&abstract_name, &authkey, &req)
            .await
            .unwrap();
        assert_eq!(r, Value::Bool(true), "fresh blackhole returns true");

        // Now reported as blackholed.
        let r = rpc_client_call(&abstract_name, &authkey, &is_blackholed(&hash))
            .await
            .unwrap();
        assert_eq!(r, Value::Bool(true), "blackholed after insert");

        // Duplicate insert returns None (Python semantics).
        let r = rpc_client_call(&abstract_name, &authkey, &req)
            .await
            .unwrap();
        assert_eq!(r, Value::None, "duplicate blackhole returns None");

        // Listed in blackholed_identities as a dict keyed by the hash.
        let list_req = pickle_dict(vec![(
            pickle_str_key("get"),
            pickle_str("blackholed_identities"),
        )]);
        let r = rpc_client_call(&abstract_name, &authkey, &list_req)
            .await
            .unwrap();
        match &r {
            Value::Dict(m) => {
                assert_eq!(m.len(), 1, "one blackholed identity");
                assert!(
                    m.contains_key(&HashableValue::Bytes(hash.clone())),
                    "listed under its identity hash"
                );
            }
            other => panic!("expected dict, got {other:?}"),
        }

        // Unblackhole: was present -> bool true.
        let unreq = pickle_dict(vec![(
            pickle_str_key("unblackhole_identity"),
            pickle_bytes(&hash),
        )]);
        let r = rpc_client_call(&abstract_name, &authkey, &unreq)
            .await
            .unwrap();
        assert_eq!(
            r,
            Value::Bool(true),
            "lifting a present blackhole returns true"
        );

        // Gone now.
        let r = rpc_client_call(&abstract_name, &authkey, &is_blackholed(&hash))
            .await
            .unwrap();
        assert_eq!(r, Value::Bool(false), "not blackholed after removal");

        // Unblackhole again: not present -> None.
        let r = rpc_client_call(&abstract_name, &authkey, &unreq)
            .await
            .unwrap();
        assert_eq!(r, Value::None, "lifting an absent blackhole returns None");
    }

    /// Test that wrong authkey is rejected.
    #[tokio::test]
    async fn test_rpc_auth_failure() {
        let core = make_test_core(true);
        let start_time = std::time::Instant::now();
        let authkey = derive_authkey(&core);

        let instance_name = format!("rpctest_auth_{}", std::process::id());
        let abstract_name = format!("rns/{}/rpc", instance_name);

        spawn_rpc_server(
            &instance_name,
            Arc::clone(&core),
            authkey,
            start_time,
            empty_stats_map(),
            crate::interfaces::inventory::InterfaceInventory::shared(),
            AutoPeerCount::default(),
            None,
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let wrong_key = [0xFFu8; 32];
        let request = pickle_dict(vec![(pickle_str_key("get"), pickle_str("interface_stats"))]);
        let result = rpc_client_call(&abstract_name, &wrong_key, &request).await;
        assert!(result.is_err(), "wrong authkey should cause failure");
    }

    /// Test link_count RPC. The count is the size of the TRANSPORT link table
    /// (the relayed links), which a freshly-built core has none of; the
    /// populated pair is asserted in
    /// `rpc::handlers::tests::link_count_pair_answers_from_the_transport_link_table`.
    #[tokio::test]
    async fn test_rpc_link_count() {
        let core = make_test_core(false);
        let start_time = std::time::Instant::now();
        let authkey = derive_authkey(&core);

        let instance_name = format!("rpctest_lc_{}", std::process::id());
        let abstract_name = format!("rns/{}/rpc", instance_name);

        spawn_rpc_server(
            &instance_name,
            Arc::clone(&core),
            authkey,
            start_time,
            empty_stats_map(),
            crate::interfaces::inventory::InterfaceInventory::shared(),
            AutoPeerCount::default(),
            None,
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let request = pickle_dict(vec![(pickle_str_key("get"), pickle_str("link_count"))]);
        let response = rpc_client_call(&abstract_name, &authkey, &request)
            .await
            .unwrap();

        match response {
            Value::I64(count) => assert_eq!(count, 0, "no links established"),
            other => panic!("expected int, got: {:?}", other),
        }
    }

    /// `rpc_query("path_table")` must send `{"get":"path_table","max_hops":None}`
    /// (Python `rnsd` `KeyError`s on a missing `max_hops`). Verify the request
    /// round-trips against our own server and decodes to a JSON array, and that
    /// an explicit `max_hops: None` request is accepted directly.
    #[tokio::test]
    async fn test_rpc_query_path_table_sends_max_hops() {
        let core = make_test_core(true);
        let start_time = std::time::Instant::now();
        let authkey = derive_authkey(&core);

        let instance_name = format!("rpctest_pt_{}", std::process::id());

        spawn_rpc_server(
            &instance_name,
            Arc::clone(&core),
            authkey,
            start_time,
            empty_stats_map(),
            crate::interfaces::inventory::InterfaceInventory::shared(),
            AutoPeerCount::default(),
            None,
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // rpc_query builds `rns/{instance_name}/rpc` internally and, for
        // path_table, appends `max_hops: None`.
        let json = rpc_query(&instance_name, &authkey, "path_table")
            .await
            .expect("path_table query should succeed");
        assert!(
            json.is_array(),
            "path_table should decode to a JSON array, got: {json:?}"
        );

        // The explicit `max_hops: None` request shape (what rpc_query sends, and
        // what Python's client sends) is accepted by our server.
        let abstract_name = format!("rns/{}/rpc", instance_name);
        let req = pickle_dict(vec![
            (pickle_str_key("get"), pickle_str("path_table")),
            (pickle_str_key("max_hops"), Value::None),
        ]);
        let resp = rpc_client_call(&abstract_name, &authkey, &req)
            .await
            .expect("explicit max_hops:None request should round-trip");
        assert!(
            matches!(resp, Value::List(_)),
            "expected a list, got: {resp:?}"
        );
    }

    /// `rpc_client_call` must not hang forever when the peer accepts the
    /// connection but never speaks (e.g. a daemon that errored handling the
    /// request and sent no response). It should fail with a `TimedOut` error
    /// within the [`RPC_CLIENT_TIMEOUT`] window.
    ///
    /// Linux-only: the mute-peer listener is built directly on an abstract
    /// Unix socket (`std::os::linux::net::SocketAddrExt`), which is a
    /// Linux-specific API. macOS/BSD use the filesystem-socket fallback path
    /// and would need a different listener construction; the timeout logic
    /// under test is platform-independent, so gating this to Linux loses no
    /// coverage of the behaviour that matters.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_rpc_client_call_times_out_on_mute_peer() {
        use std::os::linux::net::SocketAddrExt;

        let abstract_name = format!("rns/rpc-mute-{}/rpc", std::process::id());
        let addr =
            std::os::unix::net::SocketAddr::from_abstract_name(abstract_name.as_bytes()).unwrap();
        let std_listener = std::os::unix::net::UnixListener::bind_addr(&addr).unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::UnixListener::from_std(std_listener).unwrap();

        // Accept connections and hold them open without ever writing a byte.
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                // Keep the stream alive but mute.
                tokio::spawn(async move {
                    let _held = stream;
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                });
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let request = pickle_dict(vec![(pickle_str_key("get"), pickle_str("interface_stats"))]);
        let start = std::time::Instant::now();
        let result = rpc_client_call(&abstract_name, &[0u8; 32], &request).await;
        let elapsed = start.elapsed();

        match result {
            Err(RpcError::Io(ref e)) if e.kind() == std::io::ErrorKind::TimedOut => {}
            other => panic!("expected TimedOut I/O error, got: {other:?}"),
        }
        assert!(
            elapsed >= RPC_CLIENT_TIMEOUT.saturating_sub(std::time::Duration::from_millis(500)),
            "should have waited ~{:?}, only waited {elapsed:?}",
            RPC_CLIENT_TIMEOUT
        );
        assert!(
            elapsed < RPC_CLIENT_TIMEOUT + std::time::Duration::from_secs(3),
            "took far longer than the timeout: {elapsed:?}"
        );
    }

    /// `rpc_query("link_table")` round-trips against our own server and
    /// decodes to a JSON array. Empty for a freshly-built test core that has
    /// no links; the per-row response shape is exercised in the diag-side
    /// rendering test (`leviculum-cli::diag::tests`).
    ///
    /// `link_table` is a Leviculum-only extension (Python `rnsd` has no
    /// equivalent — it exposes `link_count` only). This test guards against
    /// regressions in the request → handler → response wiring.
    #[tokio::test]
    async fn test_rpc_query_link_table_round_trip() {
        let core = make_test_core(true);
        let start_time = std::time::Instant::now();
        let authkey = derive_authkey(&core);

        let instance_name = format!("rpctest_lt_{}", std::process::id());

        spawn_rpc_server(
            &instance_name,
            Arc::clone(&core),
            authkey,
            start_time,
            empty_stats_map(),
            crate::interfaces::inventory::InterfaceInventory::shared(),
            AutoPeerCount::default(),
            None,
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let json = rpc_query(&instance_name, &authkey, "link_table")
            .await
            .expect("link_table query should succeed");
        let arr = json
            .as_array()
            .expect("link_table response should be a JSON array");
        assert!(arr.is_empty(), "fresh test core has no links: {arr:?}");
    }

    /// `rpc_query("transport_tables")` round-trips against our own server
    /// (Codeberg #174), which is the presence half of the absence-tolerance
    /// contract: an `lnsd` answers, with a real path row surviving the
    /// msgpack transcode and the hex encoding `rpc_query` applies to byte
    /// values. The refusal half — a Python `rnsd` closing the connection —
    /// is pinned in `reverse_rpc_interop_tests`.
    #[tokio::test]
    async fn test_rpc_query_transport_tables_round_trip() {
        use leviculum_core::storage_types::PathEntry;
        use leviculum_core::traits::Storage as _;

        let core = make_test_core(true);
        let start_time = std::time::Instant::now();
        let authkey = derive_authkey(&core);

        let dest = [0x5Au8; 16];
        {
            let mut guard = core.lock().unwrap();
            let now = guard.now_ms();
            guard.set_interface_name(0, "TCPInterface[peer]".into());
            guard.storage_mut().set_path(
                dest,
                PathEntry {
                    hops: 2,
                    expires_ms: now + 60_000,
                    interface_index: 0,
                    random_blobs: Vec::new(),
                    next_hop: None,
                    via_peer: None,
                },
            );
        }

        let instance_name = format!("rpctest_tt_{}", std::process::id());
        spawn_rpc_server(
            &instance_name,
            Arc::clone(&core),
            authkey,
            start_time,
            empty_stats_map(),
            crate::interfaces::inventory::InterfaceInventory::shared(),
            AutoPeerCount::default(),
            None,
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let json = rpc_query(&instance_name, &authkey, "transport_tables")
            .await
            .expect("an lnsd must answer transport_tables");

        // Every table is named even when empty, so a reader can tell an empty
        // table from a daemon that cannot answer.
        for key in [
            "path_table",
            "reverse_table",
            "link_table",
            "announce_table",
            "announce_cache",
            "tunnels",
            "local_links",
        ] {
            assert!(
                json.get(key).and_then(|v| v.as_array()).is_some(),
                "{key} must be present as an array: {json}"
            );
        }

        let paths = json["path_table"].as_array().unwrap();
        assert_eq!(paths.len(), 1, "one seeded path: {paths:?}");
        assert_eq!(paths[0]["hash"], serde_json::json!("5a".repeat(16)));
        assert_eq!(paths[0]["hops"], serde_json::json!(2));
        // Direct path: `via` is the destination hash, never null — the same
        // rule the path_table RPC follows (Transport.py:1600).
        assert_eq!(paths[0]["via"], paths[0]["hash"]);
        assert_eq!(
            paths[0]["interface"],
            serde_json::json!("TCPInterface[peer]")
        );
        assert!(
            paths[0]["expires"].as_f64().unwrap() > paths[0]["timestamp"].as_f64().unwrap(),
            "expires is later than the receipt timestamp: {:?}",
            paths[0]
        );
        assert_eq!(
            paths[0]["announce_emitted"],
            serde_json::json!(0),
            "no random blob stored, so the peer-stamped emission is 0"
        );
    }

    /// `rpc_query("discovered_interfaces")` round-trips: a record persisted into
    /// the server's discovery storage comes back over the RPC as a per-record
    /// dict with the announce fields, `status`, and the `config_entry`
    /// (Codeberg #32).
    #[tokio::test]
    async fn test_rpc_query_discovered_interfaces_round_trip() {
        use leviculum_core::discovery::{DiscoveredInterface, STAMP_SIZE};

        let td = tempfile::tempdir().unwrap();
        let di = DiscoveredInterface {
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
        };
        // Persist at the current wall-clock so the server's query (which uses
        // real `now`) sees the record as fresh rather than expired.
        let now = crate::discovery::now_unix_secs();
        crate::discovery::persist_discovered(td.path(), &di, 2, now).unwrap();

        let core = make_test_core(true);
        let start_time = std::time::Instant::now();
        let authkey = derive_authkey(&core);
        let instance_name = format!("rpctest_disc_{}", std::process::id());

        spawn_rpc_server(
            &instance_name,
            Arc::clone(&core),
            authkey,
            start_time,
            empty_stats_map(),
            crate::interfaces::inventory::InterfaceInventory::shared(),
            AutoPeerCount::default(),
            Some(td.path().to_path_buf()),
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let json = rpc_query(&instance_name, &authkey, "discovered_interfaces")
            .await
            .expect("discovered_interfaces query should succeed");
        let arr = json.as_array().expect("response should be a JSON array");
        assert_eq!(arr.len(), 1, "one persisted record");
        let rec = &arr[0];
        assert_eq!(rec["type"].as_str(), Some("RNodeInterface"));
        assert_eq!(rec["name"].as_str(), Some("Node A"));
        assert_eq!(rec["hops"].as_i64(), Some(2));
        assert_eq!(rec["value"].as_i64(), Some(15));
        assert_eq!(rec["status"].as_str(), Some("available"));
        assert_eq!(
            rec["transport_id"].as_str(),
            Some("abababababababababababababababab")
        );
        assert!(rec["config_entry"]
            .as_str()
            .unwrap()
            .contains("type = RNodeInterface"));
    }

    /// Codeberg #56: the `status` field of each per-interface dict must
    /// reflect the real `Interface::is_online()` value (sourced from the
    /// shared counters the interface task flips, L-0020), not the
    /// hardcoded `true` it used to be.
    ///
    /// Sets up a core with one named interface, marks its counters
    /// offline, queries `interface_stats`, and asserts the entry
    /// reports `status: false`. Inverse: a second core+name with the
    /// counters left online reports `status: true`.
    #[tokio::test]
    async fn test_rpc_interface_stats_status_reflects_is_online() {
        for (case, expected_status) in [("offline", false), ("online", true)] {
            let core = make_test_core(true);
            let start_time = std::time::Instant::now();
            let authkey = derive_authkey(&core);

            // Register a fake interface in the core so `core.interface_stats()`
            // returns one entry. The driver does this via `core.set_interface_name()`
            // after registering the handle — here we do it directly.
            let iface_id: usize = 4242;
            let iface_name = format!("TCPInterface[lns-#56-{case}/fake:0000]");
            {
                let mut c = core.lock().unwrap();
                c.set_interface_name(iface_id, iface_name.clone());
            }

            let stats_map = empty_stats_map();
            {
                let counters = Arc::new(crate::interfaces::InterfaceCounters::new());
                counters.set_online(expected_status);
                stats_map.lock().unwrap().insert(iface_id, counters);
            }

            let instance_name = format!("rpctest_status_{}_{case}", std::process::id());
            let abstract_name = format!("rns/{instance_name}/rpc");

            spawn_rpc_server(
                &instance_name,
                Arc::clone(&core),
                authkey,
                start_time,
                Arc::clone(&stats_map),
                crate::interfaces::inventory::InterfaceInventory::shared(),
                AutoPeerCount::default(),
                None,
            )
            .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            let request = pickle_dict(vec![(pickle_str_key("get"), pickle_str("interface_stats"))]);
            let response = rpc_client_call(&abstract_name, &authkey, &request)
                .await
                .unwrap();

            let Value::Dict(d) = &response else {
                panic!("expected dict response, got: {response:?}");
            };
            let Some(Value::List(ifaces)) = d.get(&HashableValue::String("interfaces".into()))
            else {
                panic!("response missing `interfaces` list");
            };
            assert_eq!(
                ifaces.len(),
                1,
                "expected exactly one interface in the response ({case} case), got {ifaces:?}"
            );
            let Value::Dict(iface) = &ifaces[0] else {
                panic!("interface entry not a dict: {:?}", ifaces[0]);
            };
            let status = iface
                .get(&HashableValue::String("status".into()))
                .expect("interface entry missing `status`");
            assert_eq!(
                status,
                &Value::Bool(expected_status),
                "interface {case}: status should be Bool({expected_status})"
            );
            // Also confirm the entry actually corresponds to the interface we
            // registered (the name flows through unchanged).
            assert_eq!(
                iface.get(&HashableValue::String("name".into())),
                Some(&Value::String(iface_name)),
                "interface {case}: name mismatch"
            );
        }
    }
}
