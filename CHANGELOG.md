# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Storage & Memory Lifecycle
- **Elimination of Disk Recombine on Flush:** Removed legacy on-disk database merging (`kd_store.load_all()`) in `flush_off_lock` (`storage.rs`). The memory snapshot is now written directly and atomically to persistent storage, preventing expired or deleted identities from being revived from disk and aligning with reference Python Reticulum behavior where disk recombining on persist is deprecated.
- **Strict Storage Capacity Cap at Snapshot:** Enforced `MAX_KNOWN_DESTINATIONS = 5,000` directly at flush snapshot generation in `take_flush_snapshot`, guaranteeing that the persistent database file on flash storage will strictly never exceed 5,000 entries (~650 KB).

### Documentation
- **Continuous Development & Source Code Notice:** Added prominent guidance across all language sections (EN, UK, RU) in `README.md` clarifying that cutting-edge fixes, hardware tunings, and stability patches are maintained directly in the repository source tree (`main` branch).

## [0.1.0] - 2026-09-19

### Porting & Core Adaptation
- **MIPS32 Architecture Support:** Locked the version of the `leviculum` asynchronous Rust core and patched the Cargo build configuration for cross-compilation targeting the embedded MIPS32 Big-Endian architecture (`mips-unknown-linux-musl`). Configured software floating-point emulation (`soft-float`) and statically linked `libunwind` to support older Atheros-based routers without a hardware Floating Point Unit (FPU).

### Optimization & Compilation
- **Static Library Generation:** Compiled the Rust core into a static library `libleviculum.a`. To fit within the strict memory limits of the router, applied aggressive compiler optimization (LTO - Link-Time Optimization, `opt-level = "z"`, and panic abort strategy `panic = "abort"`), eliminating all unused code via dead code elimination.

### Integration & Orchestration
- **Lightweight C Daemon:** Studied the `leviculum-ffi` Foreign Function Interface (FFI) bridge which allows C code to call Rust functions. Developed a lightweight client daemon-orchestrator in C (`clevnode.c`) that is responsible for initialization, running the infinite network event loop, and managing system resources, invoking the core engine functions from the statically linked `libleviculum.a` under the hood.
