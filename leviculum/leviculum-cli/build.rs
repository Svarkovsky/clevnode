// Compose a full version string for the three CLI binaries (lnsd, lnstest,
// lncp). Local builds get the plain crate version ("0.7.0"). CI nightly
// builds get a suffix via the LEVICULUM_BUILD_ID env var, producing
// something like "0.7.0-nightly.20260419-5a5df20". The result lands in
// env var LEVICULUM_VERSION, which the binaries pick up with env!() and
// hand to clap's #[command(version = …)] attribute.
//
// The current git HEAD hash is always appended in parentheses, e.g.
// "0.7.0 (1a2b3c…)", and also exposed raw via LEVICULUM_GIT_HASH. The
// integ harness parses the parenthesised hash out of `<bin> --version`
// and refuses to run a binary whose hash does not match the repo HEAD,
// catching a wrong-branch binary that mtime alone cannot (its mtime is
// newer than the current commit). Falls back to "unknown" without git.
fn main() {
    let pkg_version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let build_id = std::env::var("LEVICULUM_BUILD_ID").unwrap_or_default();
    let base = if build_id.is_empty() {
        pkg_version
    } else {
        format!("{pkg_version}-{build_id}")
    };
    let git_hash = git_head_hash();
    let full = format!("{base} ({git_hash})");
    println!("cargo:rustc-env=LEVICULUM_VERSION={full}");
    println!("cargo:rustc-env=LEVICULUM_GIT_HASH={git_hash}");
    println!("cargo:rerun-if-env-changed=LEVICULUM_BUILD_ID");
    // Rebuild when HEAD moves (checkout, commit) so the embedded hash
    // tracks the working tree. Paths are relative to the package root,
    // and the workspace .git lives one level up.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs");
}

/// Resolve the current git HEAD commit hash, or "unknown" if git is
/// unavailable (no repo, no git binary, detached state without commits).
fn git_head_hash() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
