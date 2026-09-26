# Changelog

All notable changes to this project will be documented in this file.

## [0.1.0] - 2026-09-19

### Porting & Core Adaptation
- **MIPS32 Architecture Support:** Locked the version of the `leviculum` asynchronous Rust core and patched the Cargo build configuration for cross-compilation targeting the embedded MIPS32 Big-Endian architecture (`mips-unknown-linux-musl`). Configured software floating-point emulation (`soft-float`) and statically linked `libunwind` to support older Atheros-based routers without a hardware Floating Point Unit (FPU).

### Optimization & Compilation
- **Static Library Generation:** Compiled the Rust core into a static library `libleviculum.a`. To fit within the strict memory limits of the router, applied aggressive compiler optimization (LTO — Link-Time Optimization, `opt-level = "z"`, and panic abort strategy `panic = "abort"`), eliminating all unused code via dead code elimination.

### Integration & Orchestration
- **Lightweight C Daemon:** Studied the `leviculum-ffi` Foreign Function Interface (FFI) bridge which allows C code to call Rust functions. Developed a lightweight client daemon-orchestrator in C (`clevnode.c`) that is responsible for initialization, running the infinite network event loop, and managing system resources, invoking the core engine functions from the statically linked `libleviculum.a` under the hood.
