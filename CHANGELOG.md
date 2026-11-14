# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Announce Traffic Shaping & Protocol Parity
- **Python RNS Announce Replay Parity (Transport.py:2519):** Removed non-standard hops-based bypasses (`packet.hops >= path.hops` and `announce_emitted == path_timebase && hops <`) in `leviculum-core/src/transport.rs`. Responsive paths now unconditionally drop duplicate announce random blobs (`DropReason::AnnounceReplay`), matching Python Reticulum reference behavior (RNS 1.6.0 `Transport.py:2519`, master `Transport.py:2522`) and eliminating duplicate announce storms ("same packet, different hops" duplicates dropped to 0).
- **Announce Queue Parity & Emission Timestamp Dedup (Transport.py:1777-1781):** Refactored `QueuedAnnounce` in `leviculum-core/src/transport.rs` to track the 40-bit big-endian `emission_timestamp` extracted from the announce packet's `random_hash`. When an announce for an already-queued destination arrives in `cap.queue`, the entry is replaced only if `emission_timestamp > existing.emitted` (strictly aligning with Python Reticulum `Transport.py:1777-1781` instead of comparing hop counts).
- **Dynamic TCP Interface Shaping & Announce Pacing:** Updated `register_dynamic_interface` in `leviculum-std/src/driver/mod.rs` to inherit `bitrate` and `announce_cap` from the listening `TcpServerInterface`. Accepted incoming TCP connections now inherit egress rate-limiting and announce holdoffs from the listener configuration (e.g. 3% announce cap at 130 kbps bitrate on this deployment), pacing the egress announce stream from 20-23/s down to ~2.3/s (well beneath the initial 3.0/s Ingress Control ceiling of standard Reticulum clients).
- **Out-of-the-Box Ingress Compatibility:** Enables standard Reticulum clients with default settings (`ingress_control = yes`) to discover and populate 60-70 paths in the first minute out of the box without requiring manual `ingress_control = no` configuration overrides.
- **Test Suite Updates:** Corrected `MAX_PATH_REQUEST_TAGS` assertion to 8_000 in `leviculum-core` unit tests, and added unit tests for `cap.queue` emission timestamp deduplication (`test_announce_cap_queue_deduplicates_by_emission_timestamp`).

### Interface Discovery & RMAP Mapping
- **Geographic Coordinates & Interface Discovery in Config:** Added support for `latitude`, `longitude`, `height` (and alias `altitude`), as well as `discovery_announce_interval` configuration keys in `ini_config.rs` and `InterfaceConfig` (`leviculum-std`).
- **Wire Descriptor Population:** Forwarded coordinate parameters from interface configuration to `InterfaceDescriptor` in `descriptor_from_config`, enabling discoverable interfaces (`discoverable = yes`) to broadcast discovery announcements on `rnstransport.discovery.interface` containing standard Reticulum msgpack keys 0x03 (`latitude`), 0x04 (`longitude`), 0x05 (`height`), and valid 16-bit PoW stamps.
- **Verification & Test Coverage:** Added unit test `test_parse_discovery_coordinates_and_rmap_fields` ensuring end-to-end config parsing, descriptor population, wire payload encoding, and decode round-trip. Verified via live network capture on wire traffic that outgoing discovery announces properly contain geographic coordinates and valid proof-of-work stamps.

### Protocol Parity & Hop Limit Wire Cutoff
- **Outbound Hop Limit Enforcement (PATHFINDER_M = 128):** Aligned packet egress and forwarding conditions in `leviculum-core/src/transport.rs` with the reference Reticulum wire specification. In Reticulum, valid on-wire hop values are strictly 0 to 127 (`PATHFINDER_M = 128`), and any wire packet with `hops >= 128` triggers an immediate `ValueError: Invalid hop count 128` on Python nodes (`Packet.py:250`).
- **Forwarding and Rebroadcast Cutoff:** Changed the forward drop threshold from `> self.config.max_hops` to `>= self.config.max_hops` across `forward_on_interface_from`, `forward_on_all_except`, `forward_on_all`, and `broadcast_announce_with_caps`. Packets that reach 128 hops upon receipt increment are preserved in the local path table as reachable at boundary distance, but are discarded on air retransmission under `DropReason::ForwardMaxHops` and never emitted onto the wire.
- **Scheduler & Discovery Path Response Guards:** Updated `poll_announces` to immediately drop targeted path responses when `hops >= 128`, and retire boundary entries from the retransmit table (`remove_announce`). Added matching wire guards in `send_discovery_path_response` and `handle_path_request` before packet serialization.
- **Regression Verification:** Added comprehensive test assertions verifying that boundary announces (`wire hops = 127 -> stored hops = 128`) are retained in the path table but cleanly dropped upon scheduled retransmission without generating wire actions. All test suites (`test_announce_at_max_hops_boundary_stored_and_rebroadcast`, `test_announce_over_max_hops_not_stored_or_rebroadcast`, `test_hop_limit_forward_drops_at_max`) pass with zero drops or panics. Confirmed 0 packets with `hops >= 128` in live telemetry over 350+ captured frames.

### Build Automation & x86_64 Optimization
- **Automated x86_64 Static Build Script:** Added `build_x86_64.sh` for one-step reproducible compilation of static `x86_64-unknown-linux-musl` binaries on Linux hosts.
- **Unwind Tables (.eh_frame) Stripping for amd64:** Explicitly set `RUSTFLAGS="-C force-unwind-tables=no -C embed-bitcode=no"` for x86_64 builds. Eliminates DWARF `.eh_frame` and `.eh_frame_hdr` table bloat that standard GNU `strip` cannot remove from static executables, shrinking the stripped x86_64 binary from 2.24 MB down to 1.77 MB (saving over 440 KB).

## [0.1.2] - 2026-10-21

### Flash Memory Protection & Ratchet Lifecycle
- **In-Memory Ratchet Storage:** Eliminated individual file write-through (`save_known_ratchet`) for received remote ratchets. All known public ratchets are now maintained strictly in-memory within `MemoryStorage` BTreeMap, consuming less than 500 KB of RAM while eliminating 9,000+ micro-files and reclaiming ~37 MB of flash disk space on embedded nodes.
- **Flash Wear Elimination:** Reduced flash drive write operations by over 99.9%, eliminating filesystem fragmentation, I/O wait latency, and USB controller contention during high-throughput announce bursts.
- **Accelerated Daemon Startup:** Bypassed filesystem directory scanning (`readdir`) of thousands of legacy ratchet files on boot, achieving near-instantaneous startup.
- **Persistent Destination Keys:** Node's own private destination ratchet keys remain safely persisted on disk for seamless identity continuity.

## [0.1.1] - 2026-10-02

### Security & Memory Safety
- **Path Traversal & Injection Hardening:** Hardened `get_page_content()` in `clevnode.c` to reject hidden dotfiles (`subpath[0] == '.'`), require `.mu` extensions, and check `snprintf` return values to prevent path truncation attacks.
- **Graceful Link Teardown Cleanup:** Added comprehensive link state and pending queue freeing in `main()` cleanup path, ensuring zero memory leaks when terminating during active transfers.
- **Static Analysis & Sanitizer Verification:** Clean run under `gcc -fanalyzer -Wall -Wextra -Werror` (0 warnings). Verified zero buffer overflows, zero undefined behaviors, and zero memory leaks under AddressSanitizer (ASan) and UndefinedBehaviorSanitizer (UBSan).

### Multi-Architecture & CI/CD
- **Automated Multi-Target CI Pipeline:** Implemented automated cross-compilation in `.github/workflows/release.yml` with pre-release verification using QEMU user-mode emulation across 6 architectures:
  - `mips-unknown-linux-musl` (MIPS Big-Endian, ath79, soft-float)
  - `mipsel-unknown-linux-musl` (MIPS Little-Endian, MediaTek MT7620/MT7621/MT7628, ramips, soft-float)
  - `armv7-unknown-linux-musleabihf` (ARMv7-A 32-bit, Raspberry Pi 2/Zero 2W, Orange Pi, hard-float)
  - `aarch64-unknown-linux-musl` (ARM64, Raspberry Pi 3/4/5, Cortex-A53/A72)
  - `x86_64-unknown-linux-musl` (x86_64 64-bit generic PC and VPS)
  - `i686-unknown-linux-musl` (i686 32-bit x86 legacy PC and thin clients)
- **Automated Distribution Packaging:** Added `scripts/package_target.sh` generating standalone distribution zip archives containing `clevnode`, `.reticulum/config`, `posts/index.mu`, `S90clevnode`, and md5 checksums for each target architecture.
- **CLI Options for Diagnostics:** Added `--help` / `-h` and `--version` / `-v` handling in `clevnode.c` before `setsid()` daemonization.

### Performance & MIPS Hardware Acceleration
- **Curve25519 Unrolled Field Arithmetic:** Added package-level release profile overrides (`opt-level = 3`) for `curve25519-dalek`, `ed25519-dalek`, and `x25519-dalek` in `leviculum/Cargo.toml`. Gives LLVM permission to unroll 32-bit limb multiplications and inversions on MIPS without FPU, delivering a 30-40% speedup on Link establishment and announce verification.
- **Zero-Overhead Packet Deduplication (FastHashBuilder):** Replaced default SipHash-1-3 in `packet_cache` and `packet_cache_prev` (`storage.rs`) with a transparent 8-byte prefix hasher (`FastHashBuilder`). Because transit packet hashes are already cryptographically uniform SHA-256 digests, extracting the first 8 bytes takes a single load instruction on MIPS, cutting packet dedup CPU overhead from ~250 cycles down to near zero.

### Architecture & Thread Optimization
- **Tokio Runtime Unification:** Consolidated the FFI event bridge and driver network I/O into a single shared Tokio runtime instance (`reticulum-node`). Eliminated the redundant second runtime (`tokio-runtime-w`), dropping an idle OS worker thread, a duplicate epoll instance, and redundant timer wheels.
- **Traffic Counter Thread Elimination:** Gated `spawn_traffic_counter` behind `#[cfg(feature = "rpc")]`, removing the 1-second waking loop and dedicated OS thread when RPC is disabled.
- **Worker Stack Capping:** Enforced a strict 128 KB stack limit (`thread_stack_size(128 * 1024)`) on the unified `reticulum-node` worker thread in `driver/mod.rs`.
- **System Resource Footprint:** Reduced constant system threads from 5-6 down to exactly 3 (C main thread, C NomadNet worker, and single Tokio worker). Virtual address space (`VmSize`) dropped from 33.2 MB to 19.8 - 25.0 MB, and memory peak spike (`VmPeak`) dropped from 47.8 MB to ~27.5 MB (saving >20 MB of peak RAM under full 24/7 load with 5,000 destinations and 18k packet dedup hashes).

### Binary Size & Dead-Weight Dependency Pruning
- **Unwind Tables (.eh_frame) Stripping:** Added `-fno-unwind-tables` and `-fno-asynchronous-unwind-tables` to `CFLAGS`, as well as `-C force-unwind-tables=no -C embed-bitcode=no` to `RUSTFLAGS` across all target architectures in CI workflows and packaging scripts. Completely eliminates ~280 KB of non-strippable allocatable `.eh_frame` exception structures from static binaries.
- **Binary Footprint:** Reduced final stripped executable size from 2.74 MB down to ~2.43 MB (saving over 310 KB of dead unwind data while keeping Curve25519 loop unrolling active).
- **Regex Engine Elimination:** Removed `features = ["env-filter"]` from workspace `tracing-subscriber`, completely eliminating `matchers`, `regex-automata`, and `regex-syntax` (~280 KB of compiled code). Migrated `event_log.rs` to static `LevelFilter`.
- **Unused Interface Module Isolation:** Isolated unconditional declarations of `kiss`, `pipe`, `rnode`, and `serial` in `interfaces/mod.rs` and `driver/mod.rs` with lightweight compile-time stubs, cutting over 5,800 lines of dead code and dropping tasks like `rnode_reconnect_task` (22.4 KB).
- **JSON Parser Isolation:** Made `serde_json` strictly optional by gating `remote_status.rs` behind `cli`/`rpc` feature flags.


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
