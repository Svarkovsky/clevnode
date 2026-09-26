use std::env;
use std::path::PathBuf;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let output_file = PathBuf::from(&crate_dir).join("leviculum.h");

    cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_config(cbindgen::Config::from_file("cbindgen.toml").unwrap())
        .generate()
        .expect("Unable to generate bindings")
        .write_to_file(&output_file);

    println!("cargo::rerun-if-changed=src");
    println!("cargo::rerun-if-changed=cbindgen.toml");

    // Set the ELF SONAME on the cdylib so consumers link against the versioned
    // libleviculum.so.0 (the 0.x series soname), matching the install layout in
    // the Makefile. Applies only when a cdylib is produced (the glibc target);
    // it is ignored on the musl default, which drops the cdylib. The default
    // linker for the gnu target is cc, which understands -Wl,-soname.
    println!("cargo::rustc-cdylib-link-arg=-Wl,-soname,libleviculum.so.0");
}
