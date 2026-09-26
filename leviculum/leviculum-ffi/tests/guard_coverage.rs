//! Enforces the no-panic invariant: every exported FFI function must run its
//! body through the `catch_unwind` guard, so a Rust panic can never unwind into
//! C (undefined behaviour). See `docs/leviculum-api-design.md` §6.
//!
//! This is the automatic backstop behind the by-hand discipline: a new
//! `#[no_mangle]` function that forgets `guard(...)` fails this test.

use std::fs;
use std::path::Path;

#[test]
fn every_ffi_function_uses_the_panic_guard() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();

    for entry in fs::read_dir(&src).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = fs::read_to_string(&path).expect("read source file");
        let file = path.file_name().unwrap().to_string_lossy().to_string();

        // `#[no_mangle]` precedes every exported FFI function and nothing else
        // (function-pointer typedefs are not annotated). Each segment after a
        // split is exactly one function body, which must contain `guard(`.
        for segment in text.split("#[no_mangle]").skip(1) {
            if segment.contains("guard(") {
                continue;
            }
            let name = segment
                .split("fn ")
                .nth(1)
                .and_then(|s| s.split('(').next())
                .unwrap_or("<unknown>")
                .trim();
            offenders.push(format!("{file}::{name}"));
        }
    }

    assert!(
        offenders.is_empty(),
        "FFI functions missing the catch_unwind guard: {offenders:?}"
    );
}
