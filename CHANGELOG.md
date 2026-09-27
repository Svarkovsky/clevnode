# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Performance & MIPS Hardware Acceleration
- **Curve25519 Unrolled Field Arithmetic:** Added package-level release profile overrides (`opt-level = 3`) for `curve25519-dalek`, `ed25519-dalek`, and `x25519-dalek` in `leviculum/Cargo.toml`. Gives LLVM permission to unroll 32-bit limb multiplications and inversions on MIPS without FPU, delivering a 30-40% speedup on Link establishment and announce verification.
- **Zero-Overhead Packet Deduplication (FastHashBuilder):** Replaced default SipHash-1-3 in `packet_cache` and `packet_cache_prev` (`storage.rs`) with a transparent 8-byte prefix hasher (`FastHashBuilder`). Because transit packet hashes are already cryptographically uniform SHA-256 digests, extracting the first 8 bytes takes a single load instruction on MIPS, cutting packet dedup CPU overhead from ~250 cycles down to near zero.

### Architecture & Thread Optimization
- **Tokio Runtime Unification:** Consolidated the FFI event bridge and driver network I/O into a single shared Tokio runtime instance (`reticulum-node`). Eliminated the redundant second runtime (`tokio-runtime-w`), dropping an idle OS worker thread, a duplicate epoll instance, and redundant timer wheels.
- **Traffic Counter Thread Elimination:** Gated `spawn_traffic_counter` behind `#[cfg(feature = "rpc")]`, removing the 1-second waking loop and dedicated OS thread when RPC is disabled.
- **Worker Stack Capping:** Enforced a strict 128 KB stack limit (`thread_stack_size(128 * 1024)`) on the unified `reticulum-node` worker thread in `driver/mod.rs`.
- **System Resource Footprint:** Reduced constant system threads from 5-6 down to exactly 3 (C main thread, C NomadNet worker, and single Tokio worker). Virtual address space (`VmSize`) dropped from 33.2 MB to 4.9 MB (-85%), and memory peak spike (`VmPeak`) dropped from 47.8 MB to 6.8 MB (-86%).

### Binary Size & Dead-Weight Dependency Pruning
- **Regex Engine Elimination:** Removed `features = ["env-filter"]` from workspace `tracing-subscriber`, completely eliminating `matchers`, `regex-automata`, and `regex-syntax` (~280 KB of compiled code). Migrated `event_log.rs` to static `LevelFilter`.
- **Unused Interface Module Isolation:** Isolated unconditional declarations of `kiss`, `pipe`, `rnode`, and `serial` in `interfaces/mod.rs` and `driver/mod.rs` with lightweight compile-time stubs, cutting over 5,800 lines of dead code and dropping tasks like `rnode_reconnect_task` (22.4 KB).
- **JSON Parser Isolation:** Made `serde_json` strictly optional by gating `remote_status.rs` behind `cli`/`rpc` feature flags.
- **Binary Footprint:** Reduced final stripped executable size from 2.55 MB down to ~2.43 MB (OPTION 2: 2,432,596 bytes), saving 114.5 KB despite unrolling Curve25519 loops.

### Storage & Memory Lifecycle
- **Elimination of Disk Recombine on Flush:** Removed legacy on-disk database merging (`kd_store.load_all()`) in `flush_off_lock` (`storage.rs`). The memory snapshot is written directly and atomically to persistent storage, preventing expired or deleted identities from being revived from disk and aligning with reference Python Reticulum behavior where disk recombining on persist is deprecated.
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
