//! The control for #345: the spacing knob lives in the LoRa interface and
//! nowhere else.
//!
//! The reviewer's requirement is that the knob change the gap on the air and
//! nothing about what the telemetry reporter does — its cadence, its
//! withhold reasons, its immediate-report path. That is not something a
//! value test can show, because the reporter's decisions cannot depend on a
//! knob they never see. What can be shown, and what would actually break if
//! a later change put the delay in the wrong layer, is the seam itself: the
//! spacer is named in the interface (`lora.rs`) and in the control-plane
//! hand-off that carries the host's value to it (`usb.rs`), and in no other
//! firmware module.
//!
//! This is also the project's interface-isolation rule in executable form —
//! only the interface knows the quirks of its medium, and the on-air gap
//! between two packets is exactly such a quirk.
//!
//! The test reads the firmware sources next to this crate. It asserts a
//! positive control first (the matcher does find the knob where the knob is)
//! before it believes any of its own "not found" answers.

use std::fs;
use std::path::{Path, PathBuf};

/// Tokens that mean a file has learned about the spacing knob.
const KNOB_TOKENS: [&str; 4] = [
    "leviculum_tx_spacing",
    "TxSpacer",
    "tx_spacing",
    "TX_SPACING",
];

/// The two modules allowed to name it: the interface that applies the gap,
/// and the serial task that hands the host's value to that interface.
const ALLOWED: [&str; 2] = ["lora.rs", "usb.rs"];

fn firmware_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../src")
}

fn names_the_knob(text: &str) -> bool {
    KNOB_TOKENS.iter().any(|token| text.contains(token))
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("unreadable directory entry").path();
        if path.is_dir() {
            found.extend(rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            found.push(path);
        }
    }
    found
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn the_matcher_finds_the_knob_where_the_knob_is() {
    // The positive control. Without it, a renamed symbol would turn the
    // seam test below into a test that passes because it searches for
    // something that no longer exists anywhere.
    let lora = read(&firmware_src().join("lora.rs"));
    assert!(
        names_the_knob(&lora),
        "lora.rs does not name the spacing knob — the seam test below would \
         then pass vacuously"
    );
    for token in KNOB_TOKENS {
        assert!(
            names_the_knob(token),
            "the matcher does not even match its own token {token}"
        );
    }
}

#[test]
fn no_firmware_module_outside_the_interface_knows_about_the_spacing() {
    let src = firmware_src();
    let mut offenders = Vec::new();
    for path in rust_files(&src) {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("a source file with no name");
        if ALLOWED.contains(&name) {
            continue;
        }
        if names_the_knob(&read(&path)) {
            offenders.push(path.display().to_string());
        }
    }
    assert!(
        offenders.is_empty(),
        "the spacing knob reached a layer that must not know about it: {offenders:?}. \
         The gap belongs to the medium, so it belongs to the interface (lora.rs); \
         anything else is a delay at the wrong end of the transmit path."
    );
}

#[test]
fn the_telemetry_reporter_and_its_cadence_policy_are_untouched_by_the_knob() {
    // The reviewer's named control, spelled at the two files that own what
    // the knob must not change: the reporter (the announce/report pair, the
    // withhold reasons, the immediate path) and the cadence policy.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (file, proof) in [
        (manifest.join("../src/telemetry.rs"), "withheld"),
        (
            manifest.join("../telemetry-policy/src/lib.rs"),
            "min_interval_ms",
        ),
    ] {
        let text = read(&file);
        // Proof that this is the file it is supposed to be, before its
        // silence about the knob is believed.
        assert!(
            text.contains(proof),
            "{}: expected to find {proof} — wrong file, so its silence proves nothing",
            file.display()
        );
        assert!(
            !names_the_knob(&text),
            "{}: the reporter side has learned about the spacing knob",
            file.display()
        );
    }

    // The cadence policy carries no dependency that could reach it either.
    let policy_manifest = read(&manifest.join("../telemetry-policy/Cargo.toml"));
    assert!(
        !policy_manifest.contains("tx-spacing"),
        "the cadence policy took a dependency on the spacing knob"
    );
}
