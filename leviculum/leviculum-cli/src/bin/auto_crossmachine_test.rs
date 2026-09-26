//! Cross-machine AutoInterface interop test binary
//!
//! Runs a 4-phase test (Discovery → Announce → Link → Data) against a Python
//! Reticulum peer on a different machine. Reports results as JSON lines on stdout.
//!
//! Usage: auto-crossmachine-test --group-id autotest --timeout 60

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use leviculum_core::node::NodeEvent;
use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::interfaces::auto_interface::AutoInterfaceConfig;

fn to_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[derive(Parser, Debug)]
#[command(name = "auto-crossmachine-test")]
#[command(about = "Cross-machine AutoInterface interop test")]
struct Args {
    /// Group ID for AutoInterface discovery
    #[arg(long, default_value = "reticulum")]
    group_id: String,

    /// Overall timeout in seconds
    #[arg(long, default_value_t = 60)]
    timeout: u64,

    /// Storage directory path (uses temp dir if not specified)
    #[arg(long)]
    storage_path: Option<PathBuf>,
}

/// JSON output for a single test phase
fn report(phase: &str, result: &str, elapsed_us: u128, extra: &[(&str, &str)]) {
    let mut obj = serde_json::Map::new();
    obj.insert("phase".into(), serde_json::Value::String(phase.into()));
    obj.insert("result".into(), serde_json::Value::String(result.into()));
    obj.insert(
        "elapsed_us".into(),
        serde_json::Value::Number(serde_json::Number::from(elapsed_us as u64)),
    );
    for (k, v) in extra {
        obj.insert((*k).into(), serde_json::Value::String((*v).into()));
    }
    println!("{}", serde_json::Value::Object(obj));
}

fn report_fail(phase: &str, error: &str, elapsed_us: u128) {
    report(phase, "fail", elapsed_us, &[("error", error)]);
}

fn report_ok(phase: &str, elapsed_us: u128, extra: &[(&str, &str)]) {
    report(phase, "ok", elapsed_us, extra);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Logging: RUST_LOG env takes precedence, default to info
    tracing_subscriber::fmt()
        .compact()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    info!(
        "Starting cross-machine AutoInterface test (group_id={}, timeout={}s)",
        args.group_id, args.timeout
    );

    // Storage: use provided path or create temp dir
    let _tmpdir; // keep tempdir alive for the duration of the test
    let storage_path = match args.storage_path {
        Some(ref p) => p.clone(),
        None => {
            _tmpdir = tempfile::tempdir()?;
            _tmpdir.path().join("storage")
        }
    };

    // Build node with AutoInterface
    let auto_config = AutoInterfaceConfig {
        group_id: args.group_id.into_bytes(),
        ..Default::default()
    };

    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_path)
        .add_auto_interface_with_config(auto_config)
        .build()
        .await?;

    node.start().await?;

    let mut events = node
        .take_event_receiver()
        .ok_or("failed to take event receiver")?;

    let mut all_passed = true;

    // Phase 1: DISCOVERY
    let phase_start = Instant::now();
    let phase_timeout = std::time::Duration::from_secs(15);

    info!("Phase 1: DISCOVERY — waiting for AutoInterface peer...");
    let discovery_ok = loop {
        if phase_start.elapsed() >= phase_timeout {
            break false;
        }
        if node.auto_interface_peer_count() >= 1 {
            break true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };

    if discovery_ok {
        let elapsed = phase_start.elapsed().as_micros();
        info!("Phase 1: DISCOVERY OK ({elapsed}us)");
        report_ok("DISCOVERY", elapsed, &[]);
    } else {
        let elapsed = phase_start.elapsed().as_micros();
        info!("Phase 1: DISCOVERY FAILED (timeout)");
        report_fail("DISCOVERY", "timeout waiting for peer discovery", elapsed);
        all_passed = false;
    }

    if !all_passed {
        node.stop().await?;
        std::process::exit(1);
    }

    // Phase 2: ANNOUNCE
    let phase_start = Instant::now();
    let phase_timeout = std::time::Duration::from_secs(15);

    info!("Phase 2: ANNOUNCE — waiting for announce from Python peer...");
    let mut dest_hash = None;
    let mut signing_key = None;

    let announce_ok = loop {
        let remaining = phase_timeout.saturating_sub(phase_start.elapsed());
        if remaining.is_zero() {
            break false;
        }

        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(NodeEvent::AnnounceReceived { announce, .. })) => {
                let dh = *announce.destination_hash();
                let pk = announce.public_key();
                // Ed25519 signing key is the last 32 bytes of the 64-byte public key
                let sk: [u8; 32] = pk[32..64].try_into().expect("slice is 32 bytes");
                let dh_hex = to_hex(dh.as_bytes());
                info!("Phase 2: received announce for dest {dh_hex}");
                dest_hash = Some(dh);
                signing_key = Some(sk);
                break true;
            }
            Ok(Some(_other)) => {
                // Ignore non-announce events, keep waiting
                continue;
            }
            Ok(None) => {
                // Event channel closed
                break false;
            }
            Err(_) => {
                // Timeout
                break false;
            }
        }
    };

    if announce_ok {
        let elapsed = phase_start.elapsed().as_micros();
        let dh_hex = to_hex(dest_hash.as_ref().expect("set above").as_bytes());
        info!("Phase 2: ANNOUNCE OK ({elapsed}us)");
        report_ok("ANNOUNCE", elapsed, &[("dest_hash", &dh_hex)]);
    } else {
        let elapsed = phase_start.elapsed().as_micros();
        info!("Phase 2: ANNOUNCE FAILED (timeout)");
        report_fail("ANNOUNCE", "timeout waiting for announce", elapsed);
        all_passed = false;
    }

    if !all_passed {
        node.stop().await?;
        std::process::exit(1);
    }

    let dest_hash = dest_hash.expect("set in phase 2");
    let signing_key = signing_key.expect("set in phase 2");

    // Phase 3: LINK
    let phase_start = Instant::now();
    let phase_timeout = std::time::Duration::from_secs(15);

    info!("Phase 3: LINK — waiting for path, then connecting...");

    // Wait for path to be available before connecting
    let path_ok = loop {
        if phase_start.elapsed() >= phase_timeout {
            break false;
        }
        if node.has_path(&dest_hash) {
            break true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };

    if !path_ok {
        let elapsed = phase_start.elapsed().as_micros();
        info!("Phase 3: LINK FAILED (no path found)");
        report_fail("LINK", "timeout waiting for path", elapsed);
        node.stop().await?;
        std::process::exit(1);
    }

    let link_handle = match node.connect(&dest_hash, &signing_key).await {
        Ok(handle) => handle,
        Err(e) => {
            let elapsed = phase_start.elapsed().as_micros();
            let err_msg = format!("connect failed: {e}");
            info!("Phase 3: LINK FAILED ({err_msg})");
            report_fail("LINK", &err_msg, elapsed);
            node.stop().await?;
            std::process::exit(1);
        }
    };

    // Wait for LinkEstablished event
    let link_ok = loop {
        let remaining = phase_timeout.saturating_sub(phase_start.elapsed());
        if remaining.is_zero() {
            break false;
        }

        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(NodeEvent::LinkEstablished { link_id, .. }))
                if link_id == *link_handle.link_id() =>
            {
                break true;
            }
            Ok(Some(NodeEvent::LinkClosed {
                link_id, reason, ..
            })) if link_id == *link_handle.link_id() => {
                let elapsed = phase_start.elapsed().as_micros();
                let err_msg = format!("link closed: {reason:?}");
                info!("Phase 3: LINK FAILED ({err_msg})");
                report_fail("LINK", &err_msg, elapsed);
                node.stop().await?;
                std::process::exit(1);
            }
            Ok(Some(_other)) => continue,
            Ok(None) => break false,
            Err(_) => break false,
        }
    };

    if link_ok {
        let elapsed = phase_start.elapsed().as_micros();
        info!("Phase 3: LINK OK ({elapsed}us)");
        report_ok("LINK", elapsed, &[]);
    } else {
        let elapsed = phase_start.elapsed().as_micros();
        info!("Phase 3: LINK FAILED (timeout waiting for establishment)");
        report_fail("LINK", "timeout waiting for link establishment", elapsed);
        all_passed = false;
    }

    if !all_passed {
        node.stop().await?;
        std::process::exit(1);
    }

    // Phase 4: DATA
    let phase_start = Instant::now();

    info!("Phase 4: DATA — sending test payload...");

    match link_handle.send(b"crossmachine-test-payload").await {
        Ok(()) => {
            let elapsed = phase_start.elapsed().as_micros();
            info!("Phase 4: DATA OK ({elapsed}us)");
            report_ok("DATA", elapsed, &[]);
        }
        Err(e) => {
            let elapsed = phase_start.elapsed().as_micros();
            let err_msg = format!("send failed: {e}");
            info!("Phase 4: DATA FAILED ({err_msg})");
            report_fail("DATA", &err_msg, elapsed);
            all_passed = false;
        }
    }

    // Let the event loop dispatch the data packet before shutting down
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Shutdown
    info!("Shutting down...");
    node.stop().await?;

    if all_passed {
        std::process::exit(0);
    } else {
        std::process::exit(1);
    }
}
