# Roadmap & Project Status

This document tracks the completed optimization milestones, current project status, and upcoming horizons for the `clevnode` stack and `leviculum` core.

---

## Current Status: Community Testing & Feedback

All core optimization targets, stability fixes, and multi-architecture porting objectives have been **successfully implemented, validated on real hardware, and released in v0.1.1**.

The project is currently in the **public deployment and community feedback stage**. We are actively collecting real-world telemetry reports, mesh operator feedback, and bug reports across diverse router SoCs and operating environments.

---

## Completed Milestones (v0.1.0 - v0.1.1)

### 1. Multi-Architecture Cross-Compilation & Automated CI/CD
- [x] Implemented automated multi-architecture pipeline (`.github/workflows/release.yml`) powered by GitHub Actions.
- [x] Official standalone static binaries compiled against `musl libc` with pre-release QEMU execution verification across **6 hardware architectures**:
  - `mips-unknown-linux-musl` (MIPS Big-Endian, Qualcomm Atheros AR9xxx/QCA95xx, OpenWrt `ath79`).
  - `mipsel-unknown-linux-musl` (MIPS Little-Endian, MediaTek/Ralink MT7620/MT7621/MT7628, OpenWrt/Keenetic `ramips`).
  - `armv7-unknown-linux-musleabihf` (ARMv7-A 32-bit, Raspberry Pi 2 / Zero 2W, Orange Pi, hard-float).
  - `aarch64-unknown-linux-musl` (ARM64 / AArch64, Raspberry Pi 3/4/5, Cortex-A53/A72, modern ARM routers/servers).
  - `x86_64-unknown-linux-musl` (64-bit Intel / AMD servers, PC, and VPS).
  - `i686-unknown-linux-musl` (32-bit x86 legacy hardware and thin clients).
- [x] Automated packaging script (`scripts/package_target.sh`) including service scripts, configs, initial NomadNet page, md5 checksums, and `build_info.txt` with exact compiler flags.

### 2. Tokio Runtime Consolidation & Thread Elimination
- [x] Eliminated duplicate Tokio runtime (`tokio-runtime-w`) in FFI layer, consolidating the event bridge and driver network I/O into a single shared Tokio worker (`reticulum-node`).
- [x] Gated background traffic counter thread (`spawn_traffic_counter`) under `#[cfg(feature = "rpc")]`, eliminating the 1-second waking loop and dedicated OS thread when RPC is disabled.
- [x] Enforced strict 128 KB thread stack boundaries (`thread_stack_size(128 * 1024)`) on the unified Tokio worker.
- [x] Reduced permanent system threads from 5-6 down to strictly **3 constant OS threads** (C main, C NomadNet worker, and single Tokio worker).
- [x] Reduced total virtual address space (`VmSize`) from 33.2 MB down to 19.8 - 25.0 MB and peak memory spike (`VmPeak`) from 47.8 MB to ~27.5 MB.

### 3. MIPS Hardware Acceleration & Low-Level Fast Hashing
- [x] Configured release profile overrides (`opt-level = 3`) for `curve25519-dalek`, `ed25519-dalek`, and `x25519-dalek`. Allows LLVM to unroll critical 32-bit field multiplications, boosting Link establishment and announce verification by 30-40% on MIPS without FPU.
- [x] Implemented zero-overhead packet deduplication (`FastHashBuilder`) in `storage.rs`, taking the first 8 bytes of SHA-256 digests in a single instruction and cutting transit packet dedup CPU overhead from ~250 cycles down to near zero.

### 4. Binary Size & Dead-Weight Dependency Pruning
- [x] Removed dynamic regex engine (`regex-automata`, `regex-syntax`, `matchers`) by disabling `env-filter` in workspace `tracing-subscriber` and migrating `event_log.rs` to static `LevelFilter`.
- [x] Isolated unused interface drivers (`kiss`, `pipe`, `rnode`, `serial`) in `interfaces/mod.rs` and `driver/mod.rs` with lightweight compile-time stubs, cutting over 5,800 lines of dead code and dropping tasks like `rnode_reconnect_task` (22.4 KB).
- [x] Isolated JSON parser (`serde_json`) and `remote_status.rs` behind `cli`/`rpc` feature flags.
- [x] Reduced stripped executable size from 2.55 MB down to ~2.43 MB.

### 5. Storage Lifecycle & Database Retention Fixes
- [x] Fixed perpetual timestamp bug in `storage.rs`: timestamps update only when an actual announce arrives (`set_identity`), mirroring Python Reticulum (`Identity.remember`).
- [x] Eliminated legacy on-disk recombination (`kd_store.load_all()`) in `flush_off_lock`, writing memory snapshots directly and atomically to flash storage.
- [x] Enforced strict storage capacity cap (`MAX_KNOWN_DESTINATIONS = 5,000`) both in memory and during flush snapshotting, preventing database inflation and capping disk footprint at ~630 KB.
- [x] Implemented startup ratchet limit (`MAX_LOADED_RATCHETS = 1,000`) and auto-pruning of expired ratchet files (> 30 days) from flash storage.

### 6. Dynamic Multi-Page Serving & Zero-Downtime Hot Reloading
- [x] Implemented 16-slot LRU page cache in C wrapper checking file `mtime` via `stat()` on every request.
- [x] Added dynamic subpath mapping (`/page/*.mu`) to `posts/` with strict path-traversal sanitization.
- [x] Background scanner registers new `.mu` files every 30 seconds without daemon restarts or route resets.

---

## Future Roadmap (Pending Community Feedback)

The following directions may be explored based on user demand and feedback:

- [ ] **Additional Architectures:**
  - RISC-V 64-bit (`riscv64gc-unknown-linux-musl`) for emerging open-hardware boards (e.g. Milk-V, VisionFive).
  - MIPS64 / Loongson if community interest arises.
- [ ] **Configurable Identity Caps:** Optional config directive in `.reticulum/config` to customize `MAX_KNOWN_DESTINATIONS` for nodes with more RAM (e.g., 256 MB or 512 MB).
- [ ] **Enhanced Telemetry Reporting:** Optional lightweight status endpoint for node health monitoring without heavy RPC dependencies.
- [ ] **Documentation & Guides:** Step-by-step setup guides for Keenetic (Entware), OpenWrt LuCI web-interface integration, and Raspberry Pi systemd service units.

---

*Feedback, bug reports, and deployment logs are welcome via GitHub Issues.*
