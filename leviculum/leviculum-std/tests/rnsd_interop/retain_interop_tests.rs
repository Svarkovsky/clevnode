//! Codeberg #84 known-destination retain interop test.
//!
//! A real Python shared-instance client drives our running lnsd's
//! `destination_data` cache-lifecycle RPC (the same ops Python's
//! `Reticulum._retain_destination_data` / `_used_destination_data` /
//! `_unretain_destination_data` forward to a shared instance,
//! Reticulum.py:1267-1307). Our daemon must answer with the SAME booleans a
//! Python rnsd would, driven by the SAME use-state machine
//! (Identity.known_destinations[dest][4], Identity.py:267-293):
//!
//!   1. retain on a KNOWN destination (heard via an injected announce) -> True,
//!   2. retain on an UNKNOWN destination -> False (negative guard),
//!   3. used on a now-retained destination -> False (a pinned entry is skipped),
//!   4. unretain -> True, and used afterwards -> True again.
//!
//! Survival-under-cache-pressure is covered exhaustively at the unit level
//! (leviculum-core clean_announce_cache tests); here we prove the observable
//! wire contract against an actual Python client.
//!
//! ## Topology
//!
//! ```text
//!   injector (raw TCP) --announce--> lnsd DUT <--RPC-- retain_driver.py (client)
//! ```
//!
//! Marked `#[ignore]`: spawns Python tooling. Run with:
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop \
//!     retain_interop -- --include-ignored --test-threads=1
//! ```

use std::time::Duration;

use rand_core::OsRng;
use tokio::net::TcpStream;

use leviculum_core::constants::MTU;
use leviculum_core::identity::Identity;
use leviculum_core::{Destination, DestinationHash, DestinationType, Direction};

use crate::common::{cleanup_config_dir, init_tracing, now_ms, send_framed};
use crate::rpc_interop_tests::{python_client_ready, run_python_tool, start_rust_daemon_with_rpc};

/// A standalone Python client that connects to the shared instance and issues
/// one `destination_data` op, printing `RESULT:True`/`RESULT:False`. It uses the
/// same `--config` contract as the vendor tools so `run_python_tool` can drive
/// it unchanged.
const RETAIN_DRIVER_PY: &str = r#"
import argparse
import RNS

parser = argparse.ArgumentParser()
parser.add_argument("--config")
parser.add_argument("op")
parser.add_argument("hash")
args = parser.parse_args()

reticulum = RNS.Reticulum(configdir=args.config)
dest_hash = bytes.fromhex(args.hash)

if args.op == "retain":
    result = reticulum._retain_destination_data(dest_hash)
elif args.op == "unretain":
    result = reticulum._unretain_destination_data(dest_hash)
elif args.op == "used":
    result = reticulum._used_destination_data(dest_hash)
else:
    raise SystemExit("unknown op: " + args.op)

print("RESULT:" + str(bool(result)))
"#;

/// Build a signed announce for a caller-supplied identity so the daemon learns
/// the destination (populates its announce cache = the "known" set retain acts
/// on). Each call uses a fresh random hash so re-announces are not deduplicated.
fn build_announce_for_identity(identity: &Identity, aspect: &str) -> (Vec<u8>, DestinationHash) {
    let mut dest = Destination::new(
        Some(identity.clone()),
        Direction::In,
        DestinationType::Single,
        "retaininterop",
        &[aspect],
    )
    .expect("create destination");
    let packet = dest
        .announce(Some(b"rt"), &mut OsRng, now_ms(), now_ms() / 1000)
        .expect("create announce");
    let mut raw = [0u8; MTU];
    let size = packet.pack(&mut raw).expect("pack announce");
    (raw[..size].to_vec(), *dest.hash())
}

/// Send one announce, paced below the #87 ingress-burst threshold (same spacing
/// fix as the blackhole/discovery interop tests) so it is not held.
async fn send_announce_paced(stream: &mut TcpStream, raw: &[u8]) {
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    send_framed(stream, raw).await;
}

/// Poll until `condition` holds or the deadline passes; returns the final
/// evaluation. Never couples an assertion to a fixed sleep.
async fn wait_until<F: FnMut() -> bool>(mut condition: F, deadline: Duration) -> bool {
    let start = tokio::time::Instant::now();
    loop {
        if condition() {
            return true;
        }
        if start.elapsed() >= deadline {
            return condition();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Write the driver script into the client config dir and run one op, returning
/// the parsed boolean result. Panics with full stdio on any protocol failure.
async fn run_retain_op(config_dir: &std::path::Path, op: &str, hash_hex: &str) -> bool {
    let script_path = config_dir.join("retain_driver.py");
    std::fs::write(&script_path, RETAIN_DRIVER_PY).expect("write retain driver script");
    let script = script_path.to_str().expect("script path utf8");

    let output = run_python_tool(script, &[op, hash_hex], config_dir).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "retain_driver.py {op} must exit 0, got status {:?} stdout {:?} stderr {:?}",
        output.status.code(),
        stdout,
        String::from_utf8_lossy(&output.stderr),
    );
    let line = stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("RESULT:"))
        .unwrap_or_else(|| panic!("no RESULT line in retain_driver.py output: {stdout:?}"));
    match line.trim() {
        "True" => true,
        "False" => false,
        other => panic!("unexpected RESULT value {other:?} from retain_driver.py"),
    }
}

/// End-to-end known-destination retain lifecycle driven by a real Python
/// shared-instance client.
#[tokio::test]
#[ignore = "spawns Python tooling; reviewer runs after the tier3"]
async fn python_client_retain_lifecycle_against_lnsd() {
    init_tracing();

    let (node, instance_name, tcp_addr, identity_bytes, _storage) =
        start_rust_daemon_with_rpc().await;
    let config_dir = python_client_ready(&instance_name, &identity_bytes).await;

    // Raw TCP injector kept open for the whole test so the interface stays
    // stable across the paced announce.
    let mut injector = TcpStream::connect(tcp_addr)
        .await
        .expect("connect injector");

    let identity = Identity::generate(&mut OsRng);
    let (raw, dest) = build_announce_for_identity(&identity, "target");
    let dest_hex = hex::encode(dest.as_bytes());

    // The destination must be UNKNOWN before we hear its announce: retain is a
    // no-op and reports False (negative guard on an unknown hash).
    assert!(
        !run_retain_op(&config_dir, "retain", &dest_hex).await,
        "retain on an unknown destination must return False"
    );

    // Inject the announce; the daemon learns the destination (announce cache
    // populated = it becomes "known").
    send_announce_paced(&mut injector, &raw).await;
    assert!(
        wait_until(|| node.has_path(&dest), Duration::from_secs(10)).await,
        "the injected announce must make the destination known (learn a path)"
    );

    // Now retain returns True for the known destination.
    assert!(
        run_retain_op(&config_dir, "retain", &dest_hex).await,
        "retain on a known destination must return True"
    );

    // used on a RETAINED destination is skipped and reports False (Python
    // _used_destination_data guards use-state < 0).
    assert!(
        !run_retain_op(&config_dir, "used", &dest_hex).await,
        "used on a retained destination must return False"
    );

    // unretain lifts the pin (True); used afterwards is a normal recency touch
    // and reports True.
    assert!(
        run_retain_op(&config_dir, "unretain", &dest_hex).await,
        "unretain on a known destination must return True"
    );
    assert!(
        run_retain_op(&config_dir, "used", &dest_hex).await,
        "used on a known, non-retained destination must return True"
    );

    cleanup_config_dir(&config_dir);
}
