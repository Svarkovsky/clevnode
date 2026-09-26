//! Reference lock for the authoritative vendored Python LXMF implementation.
//!
//! Protocol work in this crate is derived from this exact snapshot. If this
//! test fails, review the upstream diff and deliberately regenerate all
//! affected Rust golden vectors before updating these pins.

use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const LXMF_VERSION: &str = "1.1.0";
const LXMF_HEAD: &str = "795fdaa2b0777c13033787d933d1afc94a2377cb";
const VECTOR_FIXTURE_SHA256: &str =
    "8cdecd08589168f94c6d50a79750bc38288181d0658ab20fca9df64c3b5103c4";

const REFERENCE_FILES: &[(&str, &str)] = &[
    (
        "LXMF/LXMF.py",
        "23b280e47f0690d27dfe469c3117f9928363716260dc85c9266f5d91146b8e90",
    ),
    (
        "LXMF/LXMessage.py",
        "9a035d03d36e80b615edfb1dbdc44abbbccd672f4a05b0802ad4b98366278e96",
    ),
    (
        "LXMF/LXStamper.py",
        "eeeba0158546d2e9878ca485ffa4b96dd13ce3e71880d784087c1fdae22538d0",
    ),
    (
        "LXMF/LXMRouter.py",
        "9439444641247cd5da8cc4b5c980a40a2584ad82b2cc0666daa5280f1632dacd",
    ),
    (
        "LXMF/LXMPeer.py",
        "5f9f655227522b0dc9159b58b42d56952a204835c747771582f1e851ffc4a2b1",
    ),
    (
        "LXMF/Handlers.py",
        "90c087b6acaf9bd46f43f325d34b54ee009983cf47f9d0bc034a3a1aa843df25",
    ),
];

fn vendor_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../reference/LXMF")
}

#[test]
fn vendored_python_lxmf_matches_reference_snapshot() {
    let root = vendor_root();
    assert!(
        root.is_dir(),
        "Python LXMF reference is missing at {}; initialise the reference/LXMF submodule",
        root.display()
    );

    let version_path = root.join("LXMF/_version.py");
    let version_source = fs::read_to_string(&version_path).unwrap_or_else(|error| {
        panic!(
            "failed to read LXMF version metadata at {}: {error}",
            version_path.display()
        )
    });
    let expected_version_line = format!("__version__ = \"{LXMF_VERSION}\"");
    assert!(
        version_source
            .lines()
            .any(|line| line.trim() == expected_version_line),
        "vendored LXMF version drift: expected {LXMF_VERSION} at commit {LXMF_HEAD}; \
         inspect {} and regenerate compatibility vectors before updating the lock",
        version_path.display()
    );

    let output = Command::new("git")
        .args(["-C"])
        .arg(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap_or_else(|error| {
            panic!("could not execute git to verify vendored LXMF commit {LXMF_HEAD}: {error}")
        });
    assert!(
        output.status.success(),
        "git could not resolve reference/LXMF HEAD while checking reference {LXMF_HEAD}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let actual_head = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        actual_head.trim(),
        LXMF_HEAD,
        "vendored LXMF HEAD drifted from the protocol reference for version {LXMF_VERSION}; \
         review the upstream changes and regenerate compatibility vectors before updating this lock"
    );

    for (relative_path, expected_hash) in REFERENCE_FILES {
        let path = root.join(relative_path);
        let bytes = fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "reference file {} is missing or unreadable at LXMF {LXMF_VERSION} ({LXMF_HEAD}): {error}",
                path.display()
            )
        });
        let actual_hash = hex::encode(Sha256::digest(&bytes));
        assert_eq!(
            actual_hash,
            *expected_hash,
            "vendored reference drift in {relative_path} at LXMF {LXMF_VERSION} ({LXMF_HEAD}); \
             inspect the Python diff and regenerate affected Rust golden vectors before updating this hash"
        );
    }

    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../docs/src/appendix/lxmf/vectors/vectors.json");
    let fixture = fs::read(&fixture_path).unwrap_or_else(|error| {
        panic!(
            "canonical LXMF vector fixture is missing at {}: {error}",
            fixture_path.display()
        )
    });
    assert_eq!(
        hex::encode(Sha256::digest(&fixture)),
        VECTOR_FIXTURE_SHA256,
        "canonical vectors drifted from the Python-generated LXMF {LXMF_VERSION} fixture; \
         rerun gen_vectors.py, review the protocol diff, and update this lock deliberately"
    );
}
