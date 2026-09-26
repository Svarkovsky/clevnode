//! rncp-compatible file transfer over Reticulum
//!
//! Module backing the `lncp` file-transfer tool (shared instance client).

use std::collections::VecDeque;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use leviculum_std::driver::{EventReceiver, ReticulumNode};
use leviculum_std::{
    Destination, DestinationHash, DestinationType, Direction, Identity, LinkId, NodeEvent,
};

fn err(msg: impl std::fmt::Display) -> Box<dyn std::error::Error> {
    msg.to_string().into()
}

/// Resolve at `deadline` if `Some`; never resolve if `None`.
///
/// Used so the transfer loop's `select!` timeout arm is a no-op when
/// the user did not pass `-w` (the default). The deadline is absolute,
/// so re-creating this future per loop iteration still resolves at the
/// original instant.
async fn deadline_or_never(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(tokio::time::Instant::from_std(d)).await,
        None => std::future::pending::<()>().await,
    }
}

/// Load an identity from disk, or generate a new one and save it.
pub fn load_or_generate_identity(path: &Path) -> Result<Identity, Box<dyn std::error::Error>> {
    if path.exists() {
        let bytes = std::fs::read(path)?;
        Identity::from_private_key_bytes(&bytes).map_err(|e| err(format!("bad identity file: {e}")))
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use rand_core::OsRng;
        let id = Identity::generate(&mut OsRng);
        let pk = id
            .private_key_bytes()
            .map_err(|e| err(format!("identity error: {e}")))?;
        std::fs::write(path, pk)?;
        Ok(id)
    }
}

/// Establish a link to a remote destination and optionally identify.
/// Shared between run_send() (push) and run_fetch() (pull).
async fn establish_link(
    node: &ReticulumNode,
    events: &mut EventReceiver,
    destination: &str,
    identity: Option<&Identity>,
    verbose: u8,
    quiet: bool,
) -> Result<LinkId, Box<dyn std::error::Error>> {
    // PathRequest has no retry: if the first one is lost, recovery
    // depends on another announce. Budget covers LoRa's slowest
    // realistic announce cadence plus margin.
    const PATH_REQUEST_BUDGET: Duration = Duration::from_secs(120);

    // Parse destination hash
    let dest_bytes_vec = crate::hex_decode(destination).map_err(err)?;
    if dest_bytes_vec.len() != 16 {
        return Err(err(format!(
            "destination must be 32 hex characters (16 bytes), got {}",
            dest_bytes_vec.len()
        )));
    }
    let mut dest_bytes = [0u8; 16];
    dest_bytes.copy_from_slice(&dest_bytes_vec);

    // Wait for path
    let dest_hash = DestinationHash::new(dest_bytes);
    if !node.has_path(&dest_hash) {
        if !quiet {
            eprintln!("Path to {} requested", destination);
        }
        node.request_path(&dest_hash).await?;
        let path_deadline = Instant::now() + PATH_REQUEST_BUDGET;
        while !node.has_path(&dest_hash) {
            if Instant::now() > path_deadline {
                return Err(err(format!("Could not find a path to {}", destination)));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    // Get signing key from identity
    let remote_identity = node.get_identity(&dest_hash).ok_or_else(|| {
        err(format!(
            "Identity for {} not found (no announce received)",
            destination
        ))
    })?;
    let pk = remote_identity.public_key_bytes();
    let mut signing_key = [0u8; 32];
    signing_key.copy_from_slice(&pk[32..64]);

    // No outer timer: the core emits LinkClosed{Timeout} when its
    // own retry budget exhausts, which terminates the loop below.
    if !quiet {
        eprintln!("Establishing link with {}...", destination);
    }
    let _stream = node.connect(&dest_hash, &signing_key).await?;
    let link_id = loop {
        match events.recv().await {
            Some(NodeEvent::LinkEstablished { link_id, .. }) => break link_id,
            Some(NodeEvent::LinkClosed { .. }) => {
                return Err(err(format!("Could not establish link to {}", destination)));
            }
            None => {
                return Err(err("Event channel closed"));
            }
            _ => {}
        }
    };

    // Identify to the remote (if identity provided)
    if let Some(id) = identity {
        node.identify_link(&link_id, id)
            .await
            .map_err(|e| err(format!("identify failed: {e}")))?;
        if verbose > 0 && !quiet {
            eprintln!("Identity announced to remote");
        }
    }

    Ok(link_id)
}

#[allow(clippy::too_many_arguments)]
pub async fn run_send(
    node: &ReticulumNode,
    events: &mut EventReceiver,
    file_path: &str,
    destination: &str,
    timeout_secs: Option<f64>,
    verbose: u8,
    quiet: bool,
    no_compress: bool,
    sender_identity: Option<&Identity>,
    phy_rates: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Read file
    let file_path = PathBuf::from(file_path);
    if !file_path.exists() {
        return Err(err(format!("file not found: {}", file_path.display())));
    }
    let data = std::fs::read(&file_path)?;
    let filename = file_path
        .file_name()
        .ok_or_else(|| err("cannot extract filename"))?
        .to_string_lossy();

    // Encode metadata matching Python's umsgpack.packb({"name": b"..."})
    let metadata_bytes = encode_metadata(filename.as_bytes());

    let link_id =
        establish_link(node, events, destination, sender_identity, verbose, quiet).await?;

    if !quiet {
        eprintln!("Sending {} ({} bytes)...", file_path.display(), data.len());
    }
    let send_data_size = data.len() as u64;
    node.send_resource(&link_id, &data, Some(&metadata_bytes), !no_compress)
        .await?;

    let transfer_deadline: Option<Instant> =
        timeout_secs.map(|s| Instant::now() + Duration::from_secs_f64(s));
    let mut speed_tracker = SpeedTracker::new();
    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Some(NodeEvent::ResourceProgress {
                        is_sender: true, progress, transfer_size, ..
                    })
                        if !quiet => {
                            let app_bytes = progress as f64 * send_data_size as f64;
                            let phy_bytes = progress as f64 * transfer_size as f64;
                            speed_tracker.update(app_bytes, phy_bytes);
                            let transferred = app_bytes as u64;
                            let speed_str = if speed_tracker.app_speed() > 0.0 {
                                format!(" - {}", format_speed(speed_tracker.app_speed()))
                            } else {
                                String::new()
                            };
                            let phy_str = if phy_rates && speed_tracker.phy_speed() > 0.0 {
                                format!(" ({} at physical layer)", format_speed_bits(speed_tracker.phy_speed()))
                            } else {
                                String::new()
                            };
                            eprint!("\rSending {} ... {} / {} ({:.1}%){}{}",
                                file_path.display(),
                                format_size(transferred),
                                format_size(send_data_size),
                                progress * 100.0,
                                speed_str,
                                phy_str);
                        }
                    Some(NodeEvent::ResourceCompleted { is_sender: true, .. }) => {
                        if !quiet {
                            eprint!("\r");
                            eprintln!("{} copied to {}",
                                file_path.display(), destination);
                        }
                        break;
                    }
                    Some(NodeEvent::ResourceFailed {
                        is_sender: true, error, ..
                    }) => {
                        return Err(err(format!(
                            "The transfer failed: {:?}", error)));
                    }
                    Some(NodeEvent::LinkClosed { .. }) => {
                        return Err(err("The transfer failed (link closed)"));
                    }
                    None => return Err(err("Event channel closed")),
                    _ => {}
                }
            }
            _ = deadline_or_never(transfer_deadline) => {
                return Err(err("The transfer timed out"));
            }
        }
    }

    Ok(())
}

/// The fetch "found" response (0xC3) is fire-and-forget, like Python's, so a
/// single loss on a lossy link strands the client at `while not
/// request_resolved`. The serve loop re-sends it on a timer after the resource
/// proof arrives, until the link tears down or this many re-sends are spent.
/// At 30% independent loss the first send plus the post-proof re-send plus
/// `MAX_RESPONSE_RESENDS` periodic re-sends give 0.3^(2+12) ~= 5e-8 residual,
/// while the cap bounds re-sends to a link whose client left without closing.
const RESPONSE_RESEND_INTERVAL: Duration = Duration::from_secs(2);
const MAX_RESPONSE_RESENDS: u32 = 12;

struct FetchEntry {
    request_id: [u8; 16],
    proof_received: bool,
    resends_left: u32,
}

/// Tracks in-flight fetch responses per link so the serve loop can re-send the
/// 0xC3 response after the resource proof arrives. Pure bookkeeping, no I/O, so
/// the re-send policy is unit-testable without a network.
#[derive(Default)]
struct FetchResponder {
    pending: std::collections::HashMap<LinkId, FetchEntry>,
}

impl FetchResponder {
    /// Record that a fetch response was sent on `link_id`, so it can be
    /// re-sent once the resource transfer is proven delivered.
    fn track(&mut self, link_id: LinkId, request_id: [u8; 16]) {
        self.pending.insert(
            link_id,
            FetchEntry {
                request_id,
                proof_received: false,
                resends_left: MAX_RESPONSE_RESENDS,
            },
        );
    }

    /// Mark the resource proof as received for `link_id`. Returns the
    /// request_id to re-send the response for immediately (the client now
    /// provably has the file but may still be missing the response).
    fn on_proof(&mut self, link_id: &LinkId) -> Option<[u8; 16]> {
        let entry = self.pending.get_mut(link_id)?;
        entry.proof_received = true;
        Some(entry.request_id)
    }

    /// Targets to re-send on a periodic tick: links whose proof has arrived and
    /// that still have re-sends left. Decrements the per-link budget.
    fn resend_targets(&mut self) -> Vec<(LinkId, [u8; 16])> {
        let mut out = Vec::new();
        for (link_id, entry) in self.pending.iter_mut() {
            if entry.proof_received && entry.resends_left > 0 {
                entry.resends_left -= 1;
                out.push((*link_id, entry.request_id));
            }
        }
        out
    }

    /// Stop tracking a link (its client tore down, or the transfer failed).
    fn forget(&mut self, link_id: &LinkId) {
        self.pending.remove(link_id);
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_listen(
    node: &ReticulumNode,
    events: &mut EventReceiver,
    identity: Identity,
    save_dir: Option<PathBuf>,
    overwrite: bool,
    no_auth: bool,
    allowed_identities: &[[u8; 16]],
    announce_interval: i64,
    verbose: u8,
    quiet: bool,
    allow_fetch: bool,
    fetch_jail: Option<PathBuf>,
    phy_rates: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Register destination and announce
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "rncp",
        &["receive"],
    )
    .map_err(|e| err(format!("destination error: {e}")))?;
    dest.set_accepts_links(true);
    let dest_hash = *dest.hash();
    node.register_destination(dest);

    if allow_fetch {
        let fetch_policy = if no_auth || allowed_identities.is_empty() {
            leviculum_core::RequestPolicy::AllowAll
        } else {
            leviculum_core::RequestPolicy::AllowList(allowed_identities.to_vec())
        };
        node.register_request_handler(dest_hash, "fetch_file", fetch_policy);
        if !quiet {
            eprintln!("Fetch requests enabled");
            if let Some(ref jail) = fetch_jail {
                eprintln!("Fetch jail: {}", jail.display());
            }
        }
    }

    eprintln!(
        "lncp listening on {}",
        crate::hex_encode(dest_hash.as_bytes())
    );

    if announce_interval >= 0 {
        node.announce_destination(&dest_hash, None).await?;
    }

    // Announce timer for periodic re-announce
    let mut announce_timer = if announce_interval > 0 {
        let mut interval = tokio::time::interval(Duration::from_secs(announce_interval as u64));
        interval.tick().await; // consume the immediate first tick
        Some(interval)
    } else {
        None
    };

    // Multi-segment accumulation buffer
    let mut segment_buffer: Vec<u8> = Vec::new();
    let mut segment_metadata: Option<Vec<u8>> = None;
    let mut speed_tracker = SpeedTracker::new();

    // Re-send the fetch 0xC3 response after the resource proof arrives so a
    // loss on a lossy link is recovered (Weg B).
    let mut fetch_responder = FetchResponder::default();
    let mut response_resend = tokio::time::interval(RESPONSE_RESEND_INTERVAL);
    response_resend.tick().await; // consume the immediate first tick

    // Event loop
    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Some(NodeEvent::LinkEstablished {
                        link_id, is_initiator: false, ..
                    }) => {
                        // Incoming links are auto-accepted and proved by the core
                        // (Python parity); the responder just configures the link
                        // here, there is no separate accept step.
                        if no_auth {
                            node.set_resource_strategy(
                                &link_id,
                                leviculum_core::resource::ResourceStrategy::AcceptAll,
                            )?;
                        }
                        if verbose > 0 {
                            eprintln!("Link established");
                        }
                    }
                    Some(NodeEvent::LinkIdentified { link_id, identity_hash }) => {
                        if allowed_identities.is_empty()
                            || allowed_identities.contains(&identity_hash)
                        {
                            node.set_resource_strategy(
                                &link_id,
                                leviculum_core::resource::ResourceStrategy::AcceptAll,
                            )?;
                            if verbose > 0 {
                                let hash_hex = crate::hex_encode(&identity_hash);
                                eprintln!("Identity {} authorized", hash_hex);
                            }
                        } else {
                            let hash_hex = crate::hex_encode(&identity_hash);
                            if !quiet {
                                eprintln!("Identity {} not allowed, tearing down link", hash_hex);
                            }
                            node.close_link(&link_id).await?;
                        }
                    }
                    Some(NodeEvent::ResourceProgress {
                        is_sender: false, progress, transfer_size, data_size, ..
                    })
                        if !quiet => {
                            let app_bytes = progress as f64 * data_size as f64;
                            let phy_bytes = progress as f64 * transfer_size as f64;
                            speed_tracker.update(app_bytes, phy_bytes);
                            let transferred = app_bytes as u64;
                            let speed_str = if speed_tracker.app_speed() > 0.0 {
                                format!(" - {}", format_speed(speed_tracker.app_speed()))
                            } else {
                                String::new()
                            };
                            let phy_str = if phy_rates && speed_tracker.phy_speed() > 0.0 {
                                format!(" ({} at physical layer)", format_speed_bits(speed_tracker.phy_speed()))
                            } else {
                                String::new()
                            };
                            eprint!("\rReceiving ... {} / {} ({:.1}%){}{}",
                                format_size(transferred),
                                format_size(data_size),
                                progress * 100.0,
                                speed_str,
                                phy_str);
                        }
                    Some(NodeEvent::ResourceCompleted {
                        data, metadata, is_sender: false,
                        segment_index, total_segments, ..
                    }) => {
                        speed_tracker = SpeedTracker::new();
                        if !quiet {
                            eprint!("\r");
                        }
                        // Accumulate segment data
                        segment_buffer.extend_from_slice(&data);
                        if metadata.is_some() {
                            segment_metadata = metadata;
                        }

                        if segment_index == total_segments {
                            // Last segment, save the complete file
                            if let Err(e) = save_received_file(
                                &segment_buffer,
                                segment_metadata.as_deref(),
                                save_dir.as_deref(),
                                overwrite,
                                quiet,
                            ) {
                                eprintln!("Error saving file: {e}");
                            }
                            segment_buffer.clear();
                            segment_metadata = None;
                        } else if verbose > 0 {
                            eprintln!(
                                "Segment {}/{} received ({} bytes)",
                                segment_index, total_segments, data.len()
                            );
                        }
                    }
                    // Fetch sender side: the resource proof arrived, so the
                    // client provably has the file. Re-send the 0xC3 response
                    // now in case the first one was lost; the periodic timer
                    // keeps re-sending until the link tears down.
                    Some(NodeEvent::ResourceCompleted { is_sender: true, link_id, .. }) => {
                        if let Some(rid) = fetch_responder.on_proof(&link_id) {
                            let _ = node.send_response(&link_id, &rid, &[0xC3]).await;
                        }
                    }
                    Some(NodeEvent::ResourceFailed { is_sender: true, link_id, error, .. }) => {
                        fetch_responder.forget(&link_id);
                        if verbose > 0 {
                            eprintln!("Transfer failed: {:?}", error);
                        }
                    }
                    Some(NodeEvent::ResourceFailed { error, .. })
                        if verbose > 0 => {
                            eprintln!("Transfer failed: {:?}", error);
                        }
                    Some(NodeEvent::LinkClosed { link_id, .. }) => {
                        // Client tore down (rncp does this on success), so stop
                        // re-sending its fetch response.
                        fetch_responder.forget(&link_id);
                        if verbose > 0 {
                            eprintln!("Link closed");
                        }
                    }
                    Some(NodeEvent::RequestReceived {
                        link_id, request_id, path, data, ..
                    }) if path == "fetch_file" => {
                        match handle_fetch_request(
                            node, &link_id, &request_id, &data,
                            fetch_jail.as_deref(), quiet, verbose,
                        ).await {
                            Ok(Some(rid)) => fetch_responder.track(link_id, rid),
                            Ok(None) => {}
                            Err(e) => {
                                if !quiet { eprintln!("Fetch error: {e}"); }
                            }
                        }
                    }
                    None => break,
                    _ => {}
                }
            }
            _ = async { announce_timer.as_mut().unwrap().tick().await },
                if announce_timer.is_some() => {
                let _ = node.announce_destination(&dest_hash, None).await;
            }
            _ = response_resend.tick() => {
                // Re-send the fetch response for every link whose resource is
                // proven delivered, recovering a response lost more than once.
                for (link_id, rid) in fetch_responder.resend_targets() {
                    let _ = node.send_response(&link_id, &rid, &[0xC3]).await;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("Shutting down");
                break;
            }
        }
    }

    Ok(())
}

/// Server-side handler for fetch_file requests.
///
/// Reads the requested file, starts a Resource transfer, then sends the
/// response. This ordering matches Python rncp exactly, the Resource ADV
/// is sent before the response so the client (which has AcceptAll set)
/// receives the data as soon as possible.
///
/// On the file-found success path the file is sent as a normal Resource
/// followed by the 0xC3 response, and the request_id is returned so the serve
/// loop can re-send the (fire-and-forget) response after the resource proof
/// arrives, recovering a response lost on a lossy link. Returns `Ok(None)` for
/// the not-found and not-allowed paths (their own response is sent here and no
/// resource transfer follows).
#[allow(clippy::too_many_arguments)]
async fn handle_fetch_request(
    node: &ReticulumNode,
    link_id: &LinkId,
    request_id: &[u8; 16],
    data: &[u8],
    fetch_jail: Option<&Path>,
    quiet: bool,
    verbose: u8,
) -> Result<Option<[u8; 16]>, Box<dyn std::error::Error>> {
    // Decode file path from msgpack string
    let requested_path = {
        let mut cursor = Cursor::new(data);
        let value = rmpv::decode::read_value(&mut cursor)
            .map_err(|e| err(format!("invalid request data: {e}")))?;
        match value {
            rmpv::Value::String(s) => s
                .into_str()
                .ok_or_else(|| err("request path is not valid UTF-8"))?,
            _ => return Err(err("request data is not a string")),
        }
    };

    if verbose > 0 && !quiet {
        eprintln!("Fetch request for: {}", requested_path);
    }

    let file_path = PathBuf::from(&requested_path);

    // Validate against jail if set
    if let Some(jail) = fetch_jail {
        let canonical_jail = std::fs::canonicalize(jail)
            .map_err(|e| err(format!("cannot resolve jail path: {e}")))?;
        let canonical_file = match std::fs::canonicalize(&file_path) {
            Ok(p) => p,
            Err(_) => {
                // File doesn't exist or can't be resolved, deny
                if !quiet {
                    eprintln!("Fetch denied (not in jail): {}", requested_path);
                }
                // Response: 0xF0 = not allowed
                node.send_response(link_id, request_id, &encode_fetch_response_not_allowed())
                    .await?;
                return Ok(None);
            }
        };
        if !canonical_file.starts_with(&canonical_jail) {
            if !quiet {
                eprintln!("Fetch denied (outside jail): {}", requested_path);
            }
            node.send_response(link_id, request_id, &encode_fetch_response_not_allowed())
                .await?;
            return Ok(None);
        }
    }

    // Check file exists
    if !file_path.exists() || !file_path.is_file() {
        if !quiet {
            eprintln!("Fetch: file not found: {}", requested_path);
        }
        // Response: false = not found
        node.send_response(link_id, request_id, &[0xC2]).await?;
        return Ok(None);
    }

    // Read file
    let file_data = std::fs::read(&file_path)
        .map_err(|e| err(format!("cannot read {}: {e}", requested_path)))?;

    // Build metadata with filename
    let filename = file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let metadata_bytes = encode_metadata(filename.as_bytes());

    if !quiet {
        eprintln!("Sending {} ({} bytes)", requested_path, file_data.len());
    }

    // Send Resource FIRST, then response, matches Python ordering
    node.send_resource(link_id, &file_data, Some(&metadata_bytes), true)
        .await?;

    // Response: true = found, transfer started. This first response is
    // fire-and-forget (Python parity); the serve loop re-sends it after the
    // resource proof arrives so a loss on the way is recovered.
    node.send_response(link_id, request_id, &[0xC3]).await?;

    Ok(Some(*request_id))
}

/// Encode the "not allowed" fetch response (msgpack uint8 0xF0).
fn encode_fetch_response_not_allowed() -> Vec<u8> {
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &rmpv::Value::Integer(0xF0_u64.into()))
        .expect("msgpack encoding to Vec cannot fail");
    buf
}

/// Decode the server's fetch response.
enum FetchResponse {
    Found,
    NotFound,
    NotAllowed,
    RemoteError,
}

fn decode_fetch_response(data: &[u8]) -> FetchResponse {
    let value = rmpv::decode::read_value(&mut Cursor::new(data)).ok();
    match value {
        Some(rmpv::Value::Boolean(true)) => FetchResponse::Found,
        Some(rmpv::Value::Boolean(false)) => FetchResponse::NotFound,
        Some(rmpv::Value::Integer(n)) if n.as_u64() == Some(0xF0) => FetchResponse::NotAllowed,
        _ => FetchResponse::RemoteError,
    }
}

/// Encode a string as msgpack for use as request data.
fn encode_msgpack_string(s: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &rmpv::Value::String(s.into()))
        .expect("msgpack encoding to Vec cannot fail");
    buf
}

/// Fetch a file from a remote listener.
#[allow(clippy::too_many_arguments)]
pub async fn run_fetch(
    node: &ReticulumNode,
    events: &mut EventReceiver,
    remote_path: &str,
    destination: &str,
    save_dir: Option<PathBuf>,
    overwrite: bool,
    timeout_secs: Option<f64>,
    verbose: u8,
    quiet: bool,
    no_compress: bool,
    sender_identity: Option<&Identity>,
    phy_rates: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = no_compress; // fetch compression is controlled by the server

    let link_id =
        establish_link(node, events, destination, sender_identity, verbose, quiet).await?;

    // Set AcceptAll BEFORE sending request, the server starts the Resource
    // transfer as a side-effect, so we must be ready to accept it.
    node.set_resource_strategy(
        &link_id,
        leviculum_core::resource::ResourceStrategy::AcceptAll,
    )?;

    let request_data = encode_msgpack_string(remote_path);
    let request_id = node
        .send_request(&link_id, "fetch_file", Some(&request_data), None)
        .await
        .map_err(|e| err(format!("send_request failed: {e}")))?;

    if !quiet {
        eprintln!("Fetch requested: {}", remote_path);
    }

    let transfer_deadline: Option<Instant> =
        timeout_secs.map(|s| Instant::now() + Duration::from_secs_f64(s));
    let mut got_response = false;
    let mut segment_buffer: Vec<u8> = Vec::new();
    let mut segment_metadata: Option<Vec<u8>> = None;
    let mut speed_tracker = SpeedTracker::new();

    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Some(NodeEvent::ResponseReceived {
                        request_id: rid, response_data, ..
                    }) if rid == request_id => {
                        match decode_fetch_response(&response_data) {
                            FetchResponse::Found => {
                                if verbose > 0 && !quiet {
                                    eprintln!("Remote has the file, receiving...");
                                }
                                got_response = true;
                            }
                            FetchResponse::NotFound => {
                                return Err(err(format!(
                                    "Fetch failed: file not found on remote: {}", remote_path)));
                            }
                            FetchResponse::NotAllowed => {
                                return Err(err(
                                    "Fetch failed: not allowed by remote".to_string()));
                            }
                            FetchResponse::RemoteError => {
                                return Err(err(
                                    "Fetch failed: remote error".to_string()));
                            }
                        }
                    }
                    Some(NodeEvent::RequestTimedOut {
                        request_id: rid, ..
                    }) if rid == request_id => {
                        return Err(err("Fetch request timed out"));
                    }
                    Some(NodeEvent::ResourceProgress {
                        is_sender: false, progress, transfer_size, data_size, ..
                    })
                        if !quiet => {
                            let app_bytes = progress as f64 * data_size as f64;
                            let phy_bytes = progress as f64 * transfer_size as f64;
                            speed_tracker.update(app_bytes, phy_bytes);
                            let transferred = app_bytes as u64;
                            let speed_str = if speed_tracker.app_speed() > 0.0 {
                                format!(" - {}", format_speed(speed_tracker.app_speed()))
                            } else {
                                String::new()
                            };
                            let phy_str = if phy_rates && speed_tracker.phy_speed() > 0.0 {
                                format!(" ({} at physical layer)", format_speed_bits(speed_tracker.phy_speed()))
                            } else {
                                String::new()
                            };
                            eprint!("\rFetching {} ... {} / {} ({:.1}%){}{}",
                                remote_path,
                                format_size(transferred),
                                format_size(data_size),
                                progress * 100.0,
                                speed_str,
                                phy_str);
                        }
                    Some(NodeEvent::ResourceCompleted {
                        data, metadata, is_sender: false,
                        segment_index, total_segments, ..
                    }) => {
                        if !quiet {
                            eprint!("\r");
                        }
                        segment_buffer.extend_from_slice(&data);
                        if metadata.is_some() {
                            segment_metadata = metadata;
                        }

                        if segment_index == total_segments {
                            // Last segment, save the complete file
                            save_received_file(
                                &segment_buffer,
                                segment_metadata.as_deref(),
                                save_dir.as_deref(),
                                overwrite,
                                quiet,
                            )?;
                            return Ok(());
                        } else if verbose > 0 {
                            eprintln!(
                                "Segment {}/{} received ({} bytes)",
                                segment_index, total_segments, data.len()
                            );
                        }
                    }
                    Some(NodeEvent::ResourceFailed { error, .. }) => {
                        return Err(err(format!("Fetch transfer failed: {:?}", error)));
                    }
                    Some(NodeEvent::LinkClosed { .. }) => {
                        if got_response {
                            return Err(err("Fetch failed: link closed during transfer"));
                        } else {
                            return Err(err("Fetch failed: link closed"));
                        }
                    }
                    None => return Err(err("Event channel closed")),
                    _ => {}
                }
            }
            _ = deadline_or_never(transfer_deadline) => {
                return Err(err("Fetch transfer timed out"));
            }
        }
    }
}

fn save_received_file(
    data: &[u8],
    metadata: Option<&[u8]>,
    save_dir: Option<&Path>,
    overwrite: bool,
    quiet: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let filename = metadata
        .and_then(extract_filename_from_metadata)
        .unwrap_or_else(|| "received_file".to_string());

    let base_dir = save_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let mut full_path = base_dir.join(&filename);
    if !overwrite {
        let mut counter = 0u32;
        while full_path.exists() {
            counter += 1;
            full_path = base_dir.join(format!("{}.{}", filename, counter));
        }
    }

    std::fs::write(&full_path, data)?;
    if !quiet {
        eprintln!("Received: {} ({} bytes)", full_path.display(), data.len());
    }
    Ok(())
}

/// Encode metadata as msgpack: {"name": bin(filename_bytes)}
///
/// Matches Python's `umsgpack.packb({"name": filename.encode("utf-8")})`.
fn encode_metadata(filename: &[u8]) -> Vec<u8> {
    let metadata_value = rmpv::Value::Map(vec![(
        rmpv::Value::String("name".into()),
        rmpv::Value::Binary(filename.to_vec()),
    )]);
    let mut metadata_bytes = Vec::new();
    rmpv::encode::write_value(&mut metadata_bytes, &metadata_value)
        .expect("msgpack encoding to Vec cannot fail");
    metadata_bytes
}

/// Extract filename from msgpack metadata: {"name": bin(filename_bytes)}
fn extract_filename_from_metadata(metadata_bytes: &[u8]) -> Option<String> {
    let mut cursor = Cursor::new(metadata_bytes);
    let value = rmpv::decode::read_value(&mut cursor).ok()?;
    let map = match value {
        rmpv::Value::Map(map) => map,
        _ => return None,
    };
    for (key, val) in &map {
        let key_str = match key {
            rmpv::Value::String(s) => s.as_str()?,
            _ => continue,
        };
        if key_str == "name" {
            let name_bytes = match val {
                rmpv::Value::Binary(b) => b,
                _ => continue,
            };
            let s = String::from_utf8(name_bytes.clone()).ok()?;
            // Path traversal protection: basename only
            let basename = Path::new(&s).file_name()?.to_string_lossy().into_owned();
            if basename.is_empty() {
                return None;
            }
            return Some(basename);
        }
    }
    None
}

struct SpeedTracker {
    samples: VecDeque<(Instant, f64, f64)>,
    max_samples: usize,
}

impl SpeedTracker {
    fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            max_samples: 32,
        }
    }

    fn update(&mut self, app_bytes: f64, phy_bytes: f64) {
        let now = Instant::now();
        self.samples.push_back((now, app_bytes, phy_bytes));
        while self.samples.len() > self.max_samples {
            self.samples.pop_front();
        }
    }

    fn app_speed(&self) -> f64 {
        self.speed(|s| s.1)
    }

    fn phy_speed(&self) -> f64 {
        self.speed(|s| s.2)
    }

    fn speed(&self, get_bytes: impl Fn(&(Instant, f64, f64)) -> f64) -> f64 {
        if self.samples.len() < 2 {
            return 0.0;
        }
        let first = self.samples.front().unwrap();
        let last = self.samples.back().unwrap();
        let span = last.0.duration_since(first.0).as_secs_f64();
        if span == 0.0 {
            return 0.0;
        }
        (get_bytes(last) - get_bytes(first)) / span
    }
}

fn format_speed(bytes_per_sec: f64) -> String {
    format!("{}/s", format_size(bytes_per_sec as u64))
}

fn format_speed_bits(bytes_per_sec: f64) -> String {
    let bits = bytes_per_sec * 8.0;
    if bits >= 1_000_000_000.0 {
        format!("{:.2} Gb/s", bits / 1_000_000_000.0)
    } else if bits >= 1_000_000.0 {
        format!("{:.2} Mb/s", bits / 1_000_000.0)
    } else if bits >= 1_000.0 {
        format!("{:.2} Kb/s", bits / 1_000.0)
    } else {
        format!("{:.0} b/s", bits)
    }
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1}MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{}B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviculum_std::driver::ReticulumNodeBuilder;
    use rand_core::OsRng;

    /// Compute the rncp/receive destination hash for an identity.
    fn dest_hash_for(identity: &Identity) -> DestinationHash {
        let dest = Destination::new(
            Some(identity.clone()),
            Direction::In,
            DestinationType::Single,
            "rncp",
            &["receive"],
        )
        .unwrap();
        *dest.hash()
    }

    /// Poll for a path to appear, up to a bounded timeout.
    ///
    /// `run_send` returns on sender-side completion, but the listener persists
    /// the received file from its own spawned task. Without a barrier the
    /// assertion races that task under parallel test load. Returns true once the
    /// path exists, false if the timeout elapses first.
    async fn wait_for_path(path: &Path, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if path.exists() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        path.exists()
    }

    /// Poll until `node` has learned a path to `dest_hash`, up to a bounded
    /// timeout. This is the deterministic signal a fixed announce-sleep stands
    /// in for: the listener's one-shot announce has reached the sender, so the
    /// transfer can proceed instead of racing a wall-clock guess that starves
    /// under concurrent compile load. Returns true once the path is known.
    async fn wait_for_node_path(
        node: &ReticulumNode,
        dest_hash: &DestinationHash,
        timeout: Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if node.has_path(dest_hash) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        node.has_path(dest_hash)
    }

    /// Create two connected ReticulumNode instances for testing.
    async fn setup_connected_nodes() -> (
        leviculum_std::driver::ReticulumNode,
        EventReceiver,
        leviculum_std::driver::ReticulumNode,
        EventReceiver,
        tempfile::TempDir,
    ) {
        let tmp = tempfile::tempdir().unwrap();

        // The listener binds `:0` itself and reports the kernel-assigned
        // port, so no free-port probe races the bind (Codeberg #221).
        let mut listener = ReticulumNodeBuilder::new()
            .add_tcp_server("127.0.0.1:0".parse().unwrap())
            .storage_path(tmp.path().join("listener"))
            .build()
            .await
            .unwrap();
        listener.start().await.unwrap();
        let listener_addr = *listener.tcp_listen_addrs().first().unwrap();
        let listener_events = listener.take_event_receiver().unwrap();

        let mut sender = ReticulumNodeBuilder::new()
            .add_tcp_client(listener_addr)
            .storage_path(tmp.path().join("sender"))
            .build()
            .await
            .unwrap();
        sender.start().await.unwrap();
        let sender_events = sender.take_event_receiver().unwrap();

        // Wait for TCP connection to establish
        tokio::time::sleep(Duration::from_millis(500)).await;

        (listener, listener_events, sender, sender_events, tmp)
    }

    #[tokio::test]
    async fn test_auth_allowed_transfer_succeeds() {
        let (listener_node, mut lev, sender_node, mut sev, tmp) = setup_connected_nodes().await;

        let listener_id = Identity::generate(&mut OsRng);
        let sender_id = Identity::generate(&mut OsRng);
        let allowed = vec![*sender_id.hash()];
        let dest_hash = dest_hash_for(&listener_id);

        // Create temp file to send
        let file_path = tmp.path().join("testfile.bin");
        std::fs::write(&file_path, b"hello auth test").unwrap();
        let save_dir = tmp.path().join("received");
        std::fs::create_dir(&save_dir).unwrap();

        // Spawn listener in background (run_listen loops forever)
        let listener_handle = tokio::spawn(async move {
            run_listen(
                &listener_node,
                &mut lev,
                listener_id,
                Some(save_dir),
                false,
                false, // no_auth = false — require identity
                &allowed,
                0,
                1,
                false,
                false, // allow_fetch
                None,  // fetch_jail
                false, // phy_rates
            )
            .await
            .map_err(|e| e.to_string())
        });

        // Wait for announce to propagate
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // Send with identity, should succeed
        let result = run_send(
            &sender_node,
            &mut sev,
            file_path.to_str().unwrap(),
            &crate::hex_encode(dest_hash.as_bytes()),
            Some(15.0),
            1,
            false,
            false,
            Some(&sender_id),
            false, // phy_rates
        )
        .await;
        assert!(
            result.is_ok(),
            "Transfer should succeed with authorized identity: {:?}",
            result.err()
        );

        // Verify file was saved. The listener persists from its own spawned
        // task, so wait for the file to land before asserting.
        let received = tmp.path().join("received").join("testfile.bin");
        assert!(
            wait_for_path(&received, Duration::from_secs(10)).await,
            "Received file should exist"
        );

        listener_handle.abort();
    }

    #[tokio::test]
    async fn test_auth_rejected_link_closed() {
        let (listener_node, mut lev, sender_node, mut sev, tmp) = setup_connected_nodes().await;

        let listener_id = Identity::generate(&mut OsRng);
        let sender_id = Identity::generate(&mut OsRng);
        let wrong_id = Identity::generate(&mut OsRng);
        let allowed = vec![*wrong_id.hash()]; // wrong hash — sender not allowed
        let dest_hash = dest_hash_for(&listener_id);

        // Create temp file to send
        let file_path = tmp.path().join("testfile.bin");
        std::fs::write(&file_path, b"should not arrive").unwrap();
        let save_dir = tmp.path().join("received");
        std::fs::create_dir(&save_dir).unwrap();

        // Spawn listener in background
        let listener_handle = tokio::spawn(async move {
            run_listen(
                &listener_node,
                &mut lev,
                listener_id,
                Some(save_dir),
                false,
                false, // no_auth = false — require identity
                &allowed,
                0,
                1,
                false,
                false, // allow_fetch
                None,  // fetch_jail
                false, // phy_rates
            )
            .await
            .map_err(|e| e.to_string())
        });

        // Wait for announce to propagate
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // Send with wrong identity, should fail
        let result = run_send(
            &sender_node,
            &mut sev,
            file_path.to_str().unwrap(),
            &crate::hex_encode(dest_hash.as_bytes()),
            Some(10.0),
            1,
            false,
            false,
            Some(&sender_id),
            false, // phy_rates
        )
        .await;
        assert!(
            result.is_err(),
            "Transfer should fail with unauthorized identity"
        );

        listener_handle.abort();
    }

    #[tokio::test]
    async fn test_no_auth_accepts_anyone() {
        let (listener_node, mut lev, sender_node, mut sev, tmp) = setup_connected_nodes().await;

        let listener_id = Identity::generate(&mut OsRng);
        let dest_hash = dest_hash_for(&listener_id);

        // Create temp file to send
        let file_path = tmp.path().join("testfile.bin");
        std::fs::write(&file_path, b"no auth test").unwrap();
        let save_dir = tmp.path().join("received");
        std::fs::create_dir(&save_dir).unwrap();

        // Spawn listener with no_auth=true (no identity needed)
        let listener_handle = tokio::spawn(async move {
            run_listen(
                &listener_node,
                &mut lev,
                listener_id,
                Some(save_dir),
                false,
                true, // no_auth = true
                &[],
                0,
                1,
                false,
                false, // allow_fetch
                None,  // fetch_jail
                false, // phy_rates
            )
            .await
            .map_err(|e| e.to_string())
        });

        // Wait for announce to propagate
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // Send without identity, should succeed in no_auth mode
        let result = run_send(
            &sender_node,
            &mut sev,
            file_path.to_str().unwrap(),
            &crate::hex_encode(dest_hash.as_bytes()),
            Some(15.0),
            1,
            false,
            false,
            None,  // no identity
            false, // phy_rates
        )
        .await;
        assert!(
            result.is_ok(),
            "Transfer should succeed in no_auth mode: {:?}",
            result.err()
        );

        let received = tmp.path().join("received").join("testfile.bin");
        assert!(
            wait_for_path(&received, Duration::from_secs(10)).await,
            "Received file should exist"
        );

        listener_handle.abort();
    }

    /// `run_send` with `timeout_secs = None` (the default once `-w` is
    /// optional) must run the transfer to completion without ever taking
    /// the timeout-error branch. Fast in-process transfer over TCP.
    #[tokio::test]
    async fn transfer_no_timeout_when_none() {
        let (listener_node, mut lev, sender_node, mut sev, tmp) = setup_connected_nodes().await;

        let listener_id = Identity::generate(&mut OsRng);
        let dest_hash = dest_hash_for(&listener_id);

        let file_path = tmp.path().join("testfile.bin");
        std::fs::write(&file_path, b"no timeout test").unwrap();
        let save_dir = tmp.path().join("received");
        std::fs::create_dir(&save_dir).unwrap();

        let listener_handle = tokio::spawn(async move {
            run_listen(
                &listener_node,
                &mut lev,
                listener_id,
                Some(save_dir),
                false,
                true, // no_auth = true
                &[],
                0,
                1,
                false,
                false, // allow_fetch
                None,  // fetch_jail
                false, // phy_rates
            )
            .await
            .map_err(|e| e.to_string())
        });

        // Deterministic readiness barrier: wait until the listener's one-shot
        // announce has reached the sender, rather than guessing with a fixed
        // sleep that under concurrent compile load can lapse before the path is
        // known.
        assert!(
            wait_for_node_path(&sender_node, &dest_hash, Duration::from_secs(10)).await,
            "sender should learn a path to the listener before sending"
        );

        // timeout_secs = None — no transfer-phase deadline at all.
        let result = run_send(
            &sender_node,
            &mut sev,
            file_path.to_str().unwrap(),
            &crate::hex_encode(dest_hash.as_bytes()),
            None,
            1,
            false,
            false,
            None,  // no identity
            false, // phy_rates
        )
        .await;
        assert!(
            result.is_ok(),
            "Transfer with timeout=None should complete: {:?}",
            result.err()
        );
        // Specifically must NOT have hit the timeout branch.
        if let Err(e) = &result {
            assert!(
                !e.to_string().contains("timed out"),
                "timeout=None must never produce a timeout error, got: {e}"
            );
        }
        // The listener persists the received file from its own node task, which
        // completes after run_send returns on the sender side. Await that task
        // deterministically instead of racing it (matches the sibling tests).
        let received = tmp.path().join("received").join("testfile.bin");
        assert!(
            wait_for_path(&received, Duration::from_secs(10)).await,
            "Received file should exist"
        );

        listener_handle.abort();
    }

    #[test]
    fn test_metadata_encoding() {
        let encoded = encode_metadata(b"myfile.txt");
        // fixmap(1)=0x81, fixstr(4)=0xa4, "name", bin8(10)=0xc4 0x0a, "myfile.txt"
        let expected: Vec<u8> = vec![
            0x81, // fixmap, 1 entry
            0xa4, // fixstr, length 4
            b'n', b'a', b'm', b'e', // "name"
            0xc4, 0x0a, // bin8, length 10
            b'm', b'y', b'f', b'i', b'l', b'e', b'.', b't', b'x', b't', // "myfile.txt"
        ];
        assert_eq!(encoded, expected);
    }

    #[test]
    fn test_filename_extraction() {
        let encoded = encode_metadata(b"hello.txt");
        let name = extract_filename_from_metadata(&encoded);
        assert_eq!(name, Some("hello.txt".to_string()));
    }

    #[test]
    fn test_filename_extraction_path_traversal() {
        let encoded = encode_metadata(b"../../etc/passwd");
        let name = extract_filename_from_metadata(&encoded);
        assert_eq!(name, Some("passwd".to_string()));
    }

    #[test]
    fn test_filename_fallback_empty() {
        assert_eq!(extract_filename_from_metadata(&[]), None);
    }

    #[test]
    fn test_filename_fallback_corrupt() {
        assert_eq!(extract_filename_from_metadata(&[0xff, 0xfe]), None);
    }

    #[test]
    fn test_filename_fallback_wrong_key() {
        // {"foo": bin("bar")}
        let value = rmpv::Value::Map(vec![(
            rmpv::Value::String("foo".into()),
            rmpv::Value::Binary(b"bar".to_vec()),
        )]);
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, &value).unwrap();
        assert_eq!(extract_filename_from_metadata(&buf), None);
    }

    #[test]
    fn test_filename_fallback_wrong_type() {
        // {"name": "string_not_binary"}
        let value = rmpv::Value::Map(vec![(
            rmpv::Value::String("name".into()),
            rmpv::Value::String("string_not_binary".into()),
        )]);
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, &value).unwrap();
        assert_eq!(extract_filename_from_metadata(&buf), None);
    }

    #[test]
    fn test_fetch_response_encoding() {
        // true
        assert!(matches!(
            decode_fetch_response(&[0xC3]),
            FetchResponse::Found
        ));
        // false
        assert!(matches!(
            decode_fetch_response(&[0xC2]),
            FetchResponse::NotFound
        ));
        // 0xF0 (uint8)
        let not_allowed = encode_fetch_response_not_allowed();
        assert!(matches!(
            decode_fetch_response(&not_allowed),
            FetchResponse::NotAllowed
        ));
        // garbage
        assert!(matches!(
            decode_fetch_response(&[0xFF]),
            FetchResponse::RemoteError
        ));
        // empty
        assert!(matches!(
            decode_fetch_response(&[]),
            FetchResponse::RemoteError
        ));
    }

    #[test]
    fn test_msgpack_string_encoding() {
        let encoded = encode_msgpack_string("test.txt");
        let mut cursor = Cursor::new(&encoded);
        let value = rmpv::decode::read_value(&mut cursor).unwrap();
        assert_eq!(value, rmpv::Value::String("test.txt".into()));
    }

    #[test]
    fn test_jail_validation() {
        let tmp = tempfile::tempdir().unwrap();
        let jail = tmp.path().join("allowed");
        std::fs::create_dir(&jail).unwrap();

        let inside = jail.join("file.txt");
        std::fs::write(&inside, b"content").unwrap();

        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, b"secret").unwrap();

        // File inside jail, canonicalize succeeds and starts_with matches
        let canonical_jail = std::fs::canonicalize(&jail).unwrap();
        let canonical_inside = std::fs::canonicalize(&inside).unwrap();
        assert!(canonical_inside.starts_with(&canonical_jail));

        // File outside jail, should NOT start_with jail
        let canonical_outside = std::fs::canonicalize(&outside).unwrap();
        assert!(!canonical_outside.starts_with(&canonical_jail));
    }

    #[tokio::test]
    async fn test_fetch_succeeds() {
        let (listener_node, mut lev, sender_node, mut sev, tmp) = setup_connected_nodes().await;

        let listener_id = Identity::generate(&mut OsRng);
        let dest_hash = dest_hash_for(&listener_id);

        // Create temp file to serve
        let serve_dir = tmp.path().join("serve");
        std::fs::create_dir(&serve_dir).unwrap();
        let serve_file = serve_dir.join("testfile.txt");
        std::fs::write(&serve_file, b"fetch test content").unwrap();

        let save_dir = tmp.path().join("fetched");
        std::fs::create_dir(&save_dir).unwrap();

        // Spawn listener with allow_fetch=true, no_auth=true
        let serve_file_path = serve_file.to_str().unwrap().to_string();
        let listener_handle = tokio::spawn(async move {
            run_listen(
                &listener_node,
                &mut lev,
                listener_id,
                None,
                false,
                true, // no_auth
                &[],
                0,
                1,
                false,
                true,  // allow_fetch
                None,  // no jail
                false, // phy_rates
            )
            .await
            .map_err(|e| e.to_string())
        });

        // Wait for announce to propagate
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // Fetch the file
        let result = run_fetch(
            &sender_node,
            &mut sev,
            &serve_file_path,
            &crate::hex_encode(dest_hash.as_bytes()),
            Some(save_dir.clone()),
            false,
            Some(15.0),
            1,
            false,
            false,
            None,
            false, // phy_rates
        )
        .await;
        assert!(result.is_ok(), "Fetch should succeed: {:?}", result.err());

        // Verify file was saved with correct content. The listener persists
        // the fetched file from its own spawned task, so wait for it to land.
        let saved = save_dir.join("testfile.txt");
        assert!(
            wait_for_path(&saved, Duration::from_secs(10)).await,
            "Fetched file should exist"
        );
        assert_eq!(
            std::fs::read(&saved).unwrap(),
            b"fetch test content",
            "Fetched file content should match"
        );

        listener_handle.abort();
    }

    /// Weg B regression: the fetch 0xC3 response must be (re-)sent more than
    /// once across a transfer, so a loss on a lossy link is recovered. The
    /// re-send policy lives in `FetchResponder`, tested here without a network:
    /// no re-send before the proof (avoid contending with parts), one re-send
    /// at the proof, then capped periodic re-sends, stopping on link teardown.
    #[test]
    fn fetch_response_resent_more_than_once_across_transfer() {
        let mut r = FetchResponder::default();
        let link = LinkId::from([7u8; 16]);
        let rid = [9u8; 16];

        r.track(link, rid);

        // Before the resource proof: no re-send (parts are still flowing).
        assert!(
            r.resend_targets().is_empty(),
            "must not re-send before the proof arrives"
        );

        // Proof arrives: one immediate post-proof re-send is scheduled.
        assert_eq!(r.on_proof(&link), Some(rid), "post-proof re-send expected");

        // Then the periodic timer re-sends, bounded by the cap.
        let mut periodic = 0usize;
        loop {
            let targets = r.resend_targets();
            if targets.is_empty() {
                break;
            }
            assert_eq!(targets, vec![(link, rid)]);
            periodic += 1;
            assert!(
                periodic <= MAX_RESPONSE_RESENDS as usize,
                "cap must bound periodic re-sends"
            );
        }
        assert_eq!(
            periodic, MAX_RESPONSE_RESENDS as usize,
            "exactly MAX_RESPONSE_RESENDS periodic re-sends"
        );

        // Total response transmissions across the transfer: original send (in
        // handle_fetch_request) + post-proof + periodic. More than one.
        let total_sends = 1 + 1 + periodic;
        assert!(
            total_sends > 1,
            "fetch response must be sent more than once, got {total_sends}"
        );

        // Client teardown (rncp does this on success) stops re-sending.
        r.track(link, rid);
        r.on_proof(&link);
        r.forget(&link);
        assert!(
            r.resend_targets().is_empty(),
            "no re-send after the link closed"
        );
    }

    #[tokio::test]
    async fn test_fetch_file_not_found() {
        let (listener_node, mut lev, sender_node, mut sev, tmp) = setup_connected_nodes().await;

        let listener_id = Identity::generate(&mut OsRng);
        let dest_hash = dest_hash_for(&listener_id);

        let save_dir = tmp.path().join("fetched");
        std::fs::create_dir(&save_dir).unwrap();

        // Spawn listener with allow_fetch=true, no_auth=true
        let listener_handle = tokio::spawn(async move {
            run_listen(
                &listener_node,
                &mut lev,
                listener_id,
                None,
                false,
                true, // no_auth
                &[],
                0,
                1,
                false,
                true,  // allow_fetch
                None,  // no jail
                false, // phy_rates
            )
            .await
            .map_err(|e| e.to_string())
        });

        // Wait for announce to propagate
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // Fetch a nonexistent file
        let result = run_fetch(
            &sender_node,
            &mut sev,
            "/nonexistent/file.txt",
            &crate::hex_encode(dest_hash.as_bytes()),
            Some(save_dir),
            false,
            Some(15.0),
            1,
            false,
            false,
            None,
            false, // phy_rates
        )
        .await;
        assert!(result.is_err(), "Fetch should fail for nonexistent file");
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("not found"),
            "Error should mention 'not found': {err_msg}"
        );

        listener_handle.abort();
    }

    #[tokio::test]
    async fn test_fetch_denied_by_jail() {
        let (listener_node, mut lev, sender_node, mut sev, tmp) = setup_connected_nodes().await;

        let listener_id = Identity::generate(&mut OsRng);
        let dest_hash = dest_hash_for(&listener_id);

        // Create a jail dir and a file OUTSIDE it
        let jail_dir = tmp.path().join("jail");
        std::fs::create_dir(&jail_dir).unwrap();
        let outside_file = tmp.path().join("secret.txt");
        std::fs::write(&outside_file, b"secret data").unwrap();

        let save_dir = tmp.path().join("fetched");
        std::fs::create_dir(&save_dir).unwrap();

        let outside_path = outside_file.to_str().unwrap().to_string();

        // Spawn listener with allow_fetch=true, jail set
        let listener_handle = tokio::spawn(async move {
            run_listen(
                &listener_node,
                &mut lev,
                listener_id,
                None,
                false,
                true, // no_auth
                &[],
                0,
                1,
                false,
                true,           // allow_fetch
                Some(jail_dir), // fetch_jail
                false,          // phy_rates
            )
            .await
            .map_err(|e| e.to_string())
        });

        // Wait for announce to propagate
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // Fetch a file outside the jail
        let result = run_fetch(
            &sender_node,
            &mut sev,
            &outside_path,
            &crate::hex_encode(dest_hash.as_bytes()),
            Some(save_dir),
            false,
            Some(15.0),
            1,
            false,
            false,
            None,
            false, // phy_rates
        )
        .await;
        assert!(result.is_err(), "Fetch should fail for jailed file");
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("not allowed"),
            "Error should mention 'not allowed': {err_msg}"
        );

        listener_handle.abort();
    }
}
