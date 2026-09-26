use std::process::Command;

fn main() {
    println!("cargo:rustc-link-search=.");
    emit_build_sha();
}

// Embed the short git SHA and a dirty flag so the firmware can log a
// [FW_BUILD] banner the CI runner reads back over the debug serial after
// flashing. This is the verification half of the LNode auto-flash: it
// proves the firmware on the board was built from the commit under test,
// not a stale image. Best-effort: a source tree with no reachable git
// (release tarball) yields sha "unknown" and dirty=false.
fn emit_build_sha() {
    let sha = run_git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());

    // Tracked-file modifications only. Untracked artifacts (target/, editor
    // temp files) must not flag a clean commit as dirty.
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    println!("cargo:rustc-env=LEVICULUM_GIT_SHA={sha}");
    println!("cargo:rustc-env=LEVICULUM_GIT_DIRTY={dirty}");

    // Re-run build.rs when HEAD moves so a re-flash from a new commit
    // re-embeds its SHA. Resolve the git dir explicitly to stay correct
    // under worktrees and submodules.
    if let Some(git_dir) = run_git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        println!("cargo:rerun-if-changed={git_dir}/index");
    }
    // Emitting ANY rerun-if-changed replaces cargo's default
    // "rerun on any package file change", so the sources have to be
    // listed explicitly. Without this an edit under `src/` left the
    // embedded `dirty` flag stale, and `[FW_BUILD]` then claimed a
    // clean tree for an image built from modified sources.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=memory.x");
}

fn run_git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}
