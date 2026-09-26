# The scenario suites live in the sibling `periculum` checkout, not in this
# workspace: `just extensive` and `just nightly` drive the `periculum` binary
# over its three corpora. Override the checkout with PERICULUM_ROOT or the
# binary with PERICULUM_BIN.

# Guarantee B step 1 (docs/src/concepts/checks-and-citations.md): a gate that
# runs tests records WHICH tests it executed, parsed out of the run's own
# output rather than out of `cargo test --list` — a list records intent, and a
# by-name selector that matches nothing runs zero tests and exits 0.
# `{{manifest}} <name> -- <command>` passes the command's output and exit
# status straight through and writes the manifest beside the other CI run
# state, under ~/.local/state/leviculum-ci/test-manifests/. Step 2 (not built)
# reads the union of those manifests and reports every test in none of them.
#
# It is also what keeps a gate from hanging: it waits for its child to EXIT
# rather than for the output pipe to close, kills the child's process group
# afterwards so a leaked daemon cannot hold the gate open, gives up at 1800 s
# with a named failure, and prints what it had to kill. `just standard` sat for
# two hours on 2026-08-07 for want of the first of those. Per-gate budget:
# `{{ manifest }} <name> --timeout <seconds> -- <command>`; 0 disables.
manifest := "python3 scripts/run-with-manifest.py --gate"

# Minimum-viable-reproduction tier — discipline tier, not size tier.
# Each test < 5 s, deterministic, single named failure mode.  See
# Codeberg #39 for design intent.  --test-threads=1 avoids
# port/resource contention between concurrent integration-style tests
# in the same binary.  Depends on build-integ-bins because the mvr
# tests spawn the release lnsd/lncp binaries directly.
mvr: build-integ-bins
    {{manifest}} mvr -- cargo test -p leviculum-std --test mvr -- --test-threads=1

# Promote the most-recent auto-bug bundle to the coder bridge.
# Run after a tier RED has emitted to $BRIDGE/auto-bug/instructions.md
# and you want the coder to pick it up.  See scripts/_emit-auto-bug-bundle.sh.
# BRIDGE defaults to ~/.local/state/leviculum (override LEVICULUM_BRIDGE).
# The source bundle is left in place so a re-promotion (e.g. after a
# stomped bridge) works without re-triggering the failing tier.
# One shell block so BRIDGE persists across the recipe lines.
spawn-coder:
    @BRIDGE="${LEVICULUM_BRIDGE:-$HOME/.local/state/leviculum}"; \
    if [ ! -s "$BRIDGE/auto-bug/instructions.md" ]; then \
        echo "ERROR: no auto-bug bundle at $BRIDGE/auto-bug/instructions.md"; \
        echo "       Either no recent tier RED, or the file was removed by hand."; \
        exit 1; \
    fi; \
    cp "$BRIDGE/auto-bug/instructions.md" "$BRIDGE/instructions.md"; \
    echo "[spawn-coder] bundle promoted to bridge: $BRIDGE/instructions.md"; \
    echo "[spawn-coder] source: $BRIDGE/auto-bug/instructions.md (left in place for re-promotion)"

# Build the lnflash tarball a stranger can unpack and run:
#   tar xzf lnflash-<version>.tar.gz && cd lnflash-<version> && sudo ./lnflash
# Contains the musl-static binary, our T114 firmware, Nordic's SoftDevice
# with Nordic's own licence file beside it, a manifest with checksums, and a
# user-facing README. Everything comes from this checkout — a bundle built
# out of a Meshtastic checkout is the hidden dependency our clone-and-deploy
# policy forbids. Cross-compiles the firmware, so the first run takes
# minutes; SKIP_FIRMWARE=1 reuses an existing ELF while iterating on the
# bundle itself. Output under target/lnflash/.
lnflash-bundle:
    bash scripts/lnflash-bundle.sh

# Lint the embedded firmware workspace. leviculum-nrf is its OWN cargo
# workspace — `--workspace` invocations in the repo root never reach it,
# which let 11 clippy findings accumulate unseen (audit 2026-06-11).
# Both BSP feature sets; clippy subsumes `cargo check` diagnostics.
# First run compiles the embedded deps into leviculum-nrf/target
# (minutes); warm runs are seconds.
lint-nrf:
    cd leviculum-nrf && cargo clippy --features bsp-rak4631,rak-baseboard -- -D warnings
    cd leviculum-nrf && cargo clippy --features bsp-t114 -- -D warnings
    # leviculum-screen, leviculum-sd-policy, leviculum-gnss-time,
    # leviculum-gnss-presence, leviculum-gnss-init, leviculum-telemetry-policy,
    # leviculum-ble-tx, leviculum-announce-policy, leviculum-queue-budget,
    # leviculum-log-line,
    # leviculum-tx-spacing, leviculum-rx-arming, leviculum-persist-ack,
    # leviculum-boot-trace, leviculum-channel-access, leviculum-media-state,
    # leviculum-record-log and leviculum-qspi-bitbang are the
    # pure, host-testable crates inside the leviculum-nrf workspace: clippy +
    # tests run on the host triple (the workspace's .cargo/config defaults to
    # thumbv7em).
    #
    # `--all-targets` here, unlike the two embedded feature-set lines above:
    # these crates' whole value is their host test suites, and without the flag
    # clippy lints their libs only while the `cargo test` line below merely
    # COMPILES the test code. A lint that fires solely in a test was therefore
    # invisible to every run of this recipe — the same gap the workspace line in
    # `fast` closed in e27a15e. The embedded lines stay narrow: `--all-targets`
    # there would pull in test/bench harnesses that do not link for thumbv7em.
    cd leviculum-nrf && cargo clippy -p leviculum-screen -p leviculum-sd-policy -p leviculum-gnss-time -p leviculum-gnss-presence -p leviculum-gnss-init -p leviculum-telemetry-policy -p leviculum-ble-tx -p leviculum-announce-policy -p leviculum-queue-budget -p leviculum-log-line -p leviculum-tx-spacing -p leviculum-rx-arming -p leviculum-persist-ack -p leviculum-boot-trace -p leviculum-channel-access -p leviculum-media-state -p leviculum-record-log -p leviculum-qspi-bitbang --target $(rustc -vV | sed -n 's/host: //p') --all-targets -- -D warnings
    cd leviculum-nrf && cargo test -p leviculum-screen -p leviculum-sd-policy -p leviculum-gnss-time -p leviculum-gnss-presence -p leviculum-gnss-init -p leviculum-telemetry-policy -p leviculum-ble-tx -p leviculum-announce-policy -p leviculum-queue-budget -p leviculum-log-line -p leviculum-tx-spacing -p leviculum-rx-arming -p leviculum-persist-ack -p leviculum-boot-trace -p leviculum-channel-access -p leviculum-media-state -p leviculum-record-log -p leviculum-qspi-bitbang --target $(rustc -vV | sed -n 's/host: //p')

# Stack-frame gate for the firmware. The T114 stack grows down into the
# SoftDevice RAM floor, so one oversized frame eats the whole margin and
# surfaces as an SD internal assertion rather than a clean fault. A 94 KB
# `main` frame (a by-value `NodeCore` materialised twice) did exactly that
# and left ~13 KB of margin. Reads the `sub sp` immediates out of the linked
# ELF, so it measures the shipped binary.
nrf-stack-frames:
    bash scripts/check-nrf-stack-frames.sh

# BLE event-buffer gate. nrf-softdevice sizes the `sd_ble_evt_get` buffer from
# a cargo feature, defaults to 128 bytes when none is picked, and panics rather
# than truncating when an event does not fit. Our characteristics are 251 bytes
# wide, so on the default every LNode reset within seconds of a real Android
# peer connecting (Codeberg #354). Losing the feature again is invisible: the
# firmware still builds, and the LNode-to-LNode bench negotiates an MTU small
# enough to stay under 128. Asserted against cargo's resolved feature graph.
nrf-evt-max-size:
    bash scripts/check-nrf-evt-max-size.sh

# GAP device-name pointer gate. `ble_gap_cfg_device_name_t` under
# BLE_GATTS_VLOC_STACK takes a flash pointer or NULL and nothing else; a RAM
# pointer earns NRF_ERROR_INVALID_ADDR from `sd_ble_cfg_set`, which
# nrf-softdevice turns into a panic inside `Softdevice::enable` — reached from
# `main` before its first await, so the USB task never runs and the board
# boot-loops without ever enumerating. `e52dba1` shipped exactly that and no
# other gate saw it: the name builder is pure and host-tested, both BSPs build,
# clippy is clean. Text check by necessity (the bad value is a runtime
# address); carries its own positive control.
nrf-gap-device-name:
    bash scripts/check-nrf-gap-device-name.sh

# Board pin-map gate. The two pin greps that existed before it were both
# internal-consistency checks, and `e5d62b95` passed them with the T114's QSPI
# IO2/IO3 named P1.00/P1.01 where the part has WP#/HOLD# on P0.07/P0.05: a map
# that is consistently wrong is consistent, so nothing that reads only our own
# tree can see it. Only the reference can. So this compares the QSPI and LoRa
# pins against the Meshtastic variant headers, and separately against the
# `p.P0_07` arguments the bins actually pass, which are not the aliases. (The
# T114's `id=00:00:00` was once blamed on that wrong IO3; it cannot be — the
# JEDEC read is single-line and never touches IO2/IO3. See qspi.rs §Deep power
# down.) Numbers and scope in leviculum-nrf/reference-pins.toml; the upstream
# half needs a Meshtastic checkout ($MESHTASTIC_TREE), names the revision it
# read, and says so when there is none.
nrf-board-pins:
    bash scripts/check-nrf-board-pins.sh

# SoftDevice guard for the flash runner. Our image is linked at 0x27000 and a
# factory board still carrying S140 6.1.1 forwards to 0x26000, so writing to
# one soft-bricks it (docs/src/concepts/lnode-flashing.md). The runner refuses
# that write; this drives the refusal against fixture INFO_UF2.TXT files, so
# the logic is covered without a board. That a real 6.1.1 board is refused
# stays a rig check.
nrf-sd-guard:
    bash leviculum-nrf/tools/test-softdevice-guard.sh

# Volume selection for the flash runner. The guard above decides WHETHER to
# write; this decides WHERE. It used to take the first UF2 volume in the search
# path, so one foreign board parked in its bootloader shadowed every other board
# and left its mount behind to keep doing so (Codeberg #341). Driven against
# fixture volume directories with stubbed mount/umount, so no board and no sudo.
nrf-uf2-volumes:
    bash leviculum-nrf/tools/test-uf2-volumes.sh

# Attribution for the flash runner. The guard decides WHETHER to write, the
# volume selection decides WHERE, and this decides WHO GOT IT. A UF2 volume
# carries no board serial, so the runner used to pair it with a board out of
# its own enumeration and report that one — with two T114s attached, one in
# DFU and one running, it wrote the image to the first and named the second
# (Codeberg #343, measured twice, once in each direction). Driven against
# stubbed boards, each with a firmware stamp it reports when read.
nrf-fw-readback:
    bash leviculum-nrf/tools/test-fw-readback.sh

# Static analysis for the flash-runner scripts (Codeberg #345). They have
# carried `# shellcheck` directives since they were written, so somebody once
# ran it — but nothing ever ran it again, and an SC2034 and an SC2015 sat in
# the runner unnoticed until #341 and #343 happened to remove them.
#
# scripts/flash-lnodes-from-head.sh is in the same list because it sources
# leviculum-nrf/tools/fw-readback.sh: it is part of the same source graph, and
# leaving it out would gate the module while its only non-test caller went
# unchecked.
#
# Must run from the repo root: the `source=` directives in these scripts name
# repo-relative paths, which is what lets shellcheck resolve a `.` through
# $SCRIPT_DIR. -x is what the ticket asks for and covers a future `source`
# line whose directive somebody forgets.
nrf-shellcheck:
    shellcheck -x leviculum-nrf/tools/*.sh scripts/flash-lnodes-from-head.sh \
        scripts/debug-witness.sh scripts/test-debug-witness.sh \
        scripts/device-watchdog.sh scripts/test-device-watchdog.sh \
        scripts/run-tier3-hw.sh scripts/tier3-hw-selftest.sh \
        scripts/check-nrf-evt-max-size.sh \
        scripts/check-nrf-board-pins.sh \
        scripts/check-nrf-gap-device-name.sh \
        scripts/lnode-panic-query.sh scripts/lnode-stack-reset.sh

# The tier-3 debug-port witness (Codeberg #353). Two boards on the rig have
# reset themselves mid-run for months and every occurrence was closed as
# "suspected self-reset", because the board printed its reason to nobody: the
# post-mortem is read-and-cleared at boot and nothing listened on a debug port
# during a run. This gate holds the two claims that can be settled without a
# rig — which ports get a witness, and that a reader survives losing its port
# — plus the verdict-side claim that the RED banner names the resulting file.
# No board, no periculum, no flash; the reader half runs against a pty that is
# taken away and given back.
#
# scripts/test-device-watchdog.sh joins it because the witness only explains a
# vanish somebody else decided happened, and that decision was wrong twice: a
# single failing `lsusb` poll latched a board that never moved, and periculum's
# own per-scenario board reset — a real USB disconnect we ordered — was counted
# as a device failure (Codeberg #65). Both are injected there as failures and
# asserted not to fire.
hw-witness:
    bash scripts/test-debug-witness.sh
    bash scripts/test-device-watchdog.sh
    bash scripts/tier3-hw-selftest.sh

# Rustdoc gate: broken intra-doc links fail instead of warning.
doc-gate:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# Tracing-shim gate (PR #57): leviculum-core must pass the SAME suite with
# tracing OFF as with it on. The `tracing` feature is default-on; with it
# off the level macros become no-ops via the `crate::tracing` shim in
# lib.rs. Running the full suite in that config proves the shim changed no
# core logic (a bare `tracing::x!` that slipped past the shim would either
# fail to compile here or, worse, only on M0 — see m0-build-gate).
core-no-tracing:
    {{manifest}} core-no-tracing -- cargo test -p leviculum-core --no-default-features

# Cortex-M0 gate (PR #57): leviculum-core must cross-compile for thumbv6m
# (atomic-less MCU, e.g. rp2040) with tracing off. tracing-core's CAS-based
# callsite registry does not compile there, so the default build FAILS on
# M0; --no-default-features must succeed. Keeps M0 support from rotting.
m0-build-gate:
    rustup target add thumbv6m-none-eabi
    cargo build -p leviculum-core --target thumbv6m-none-eabi --no-default-features

# Codeberg #237: leviculum-lxmf must stay buildable for the firmware triple —
# the Telemeter codec is headed for leviculum-nrf, which does not depend on
# the crate yet, so no firmware build would catch a std leak here. Default
# features off keeps `pow`/sha2 out, the configuration an embedded consumer
# would use.
lxmf-embedded-gate:
    rustup target add thumbv7em-none-eabihf
    cargo build -p leviculum-lxmf --target thumbv7em-none-eabihf --no-default-features

# Codeberg #303: run the leviculum-core lib suite on a 32-bit `usize`.
#
# NOT a firmware test. The target is i686 x86 Linux with std and an
# allocator; the ONE property it shares with thumbv7em-none-eabihf is
# `usize == u32`. Alignment, endianness-independent layout, the absent
# allocator, no_std and the SoftDevice are all different, and a green run
# here says nothing about any of them. What it does cover is the class of
# defect where a wire-supplied length is added to an offset: on 64-bit that
# arithmetic cannot wrap, so every host gate is blind to it, and #267 was
# invisible for the project's lifetime for exactly that reason — a
# `*pos + len > data.len()` guard in resource/msgpack.rs that a peer could
# wrap below `data.len()` with one packet after a link handshake.
#
# Both profiles, because they fail on different inputs:
#   debug   — overflow-checks on, so the ADDITION traps. Catches a wrap even
#             when the wrapped sum would land harmlessly inside the buffer
#             and never reach a bad slice.
#   release — overflow-checks off (see [profile.release] in Cargo.toml: it
#             does not set them), so the wrap happens and the SLICE panics.
#             That is the shipped failure mode, and it is also the only arm
#             that sees a truncation the debug trap cannot — `len as usize`
#             from a u64, or a deliberate `wrapping_add`, do not trap.
# Injecting the pre-#267 guard back into `take` on 2026-08-18 confirmed both
# arms fail on it and the x86_64 run stays green: debug panicked "attempt to
# add with overflow" at the addition, release "slice index starts at 5 but
# ends at 0" at the slice.
#
# Env vars rather than a `[target.i686-unknown-linux-musl]` section in
# .cargo/config.toml: a section there also changes what a bare `cargo build
# --target i686-...` does for everyone, and this gate should not own that.
# rust-toolchain.toml's `targets` stays as it is for the same reason the
# embedded triples are not in it — the download is forced on every checkout
# instead of on whoever runs the gate. The `rustup target add` below is how
# m0-build-gate and lxmf-embedded-gate already handle it, so a pin bump
# self-heals here.
#
# `--all-features`, not the default set. `compression` is not a default
# feature of leviculum-core, so a bare `-p leviculum-core --lib` compiles
# `#[cfg(feature = "compression")] pub mod compression` out and runs 1643 of
# the crate's 1673 lib tests. The 30 it drops are the whole compression
# module — including `resource::compression::tests::decompress_hint_is_
# clamped`, a clamp over a wire-supplied decompressed size, which is the
# defect class this gate exists for. `check-all-targets` and the workspace
# lib run do not show the gap: cargo unifies features across a workspace
# build, so another crate turns `compression` on there and the count comes
# out at 1673 either way. Prefer `--all-features` over naming `compression`
# so the next feature-gated module cannot escape the gate the same way.
# Measured on schneckenschreck 2026-08-18: 15.7 s wall warm (14.6 s debug,
# 1.1 s release — the debug arm is dominated by unoptimised bz2 roundtrips,
# 3.7 s of it the rest of the suite); cold, with the target already fetched,
# ~80 s for the two i686 builds of the crate and its deps — once per host
# per toolchain.
i686-usize-gate:
    rustup target add i686-unknown-linux-musl
    CARGO_TARGET_I686_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
    CARGO_TARGET_I686_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C link-self-contained=yes" \
    {{manifest}} i686-usize-debug -- cargo test -p leviculum-core --target i686-unknown-linux-musl --all-features --lib
    CARGO_TARGET_I686_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
    CARGO_TARGET_I686_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C link-self-contained=yes" \
    {{manifest}} i686-usize-release -- cargo test -p leviculum-core --target i686-unknown-linux-musl --all-features --release --lib

# Guarantee C step 1 (docs/src/concepts/checks-and-citations.md): the four
# vendored references must sit at the commit their gitlink names. One wrong
# fact — `reference/LXMF` twelve commits behind for five weeks — silently
# repoints every LXMF citation in the tree. Sub-second, no build, and first
# in `fast` so it reports before anything expensive: a batch that gets a
# compile error still learns its references are wrong.
#
# In a gate, not a `#[test]`, deliberately. The `reference_lock` test that
# should have caught the LXMF drift was itself red and unobserved for the
# whole five weeks; a test can be the thing that runs nowhere.
check-submodules:
    @bash scripts/check-submodule-pins.sh

# Codeberg #196: the core-processor seam must keep making the two prohibitions
# of the core-lock budget unrepresentable. Builds the leviculum-std `cf_*`
# fixtures, each of which is a `CoreProcessor` attempting one forbidden move,
# and asserts each fails to compile with one SPECIFIC error code — "does not
# compile" is not the assertion.
#
# A gate rather than a #[test] for the same reason as check-submodules, plus a
# mechanical one: shelling out to cargo from inside `cargo test` blocks on the
# build-directory lock the outer invocation holds.
check-processor-seam:
    @bash scripts/check-processor-compile-fail.sh

# Guarantee C step 3: `path:line` citations in docs/src/** and in the Rust
# sources of leviculum-core/-lxmf/-std must still point at what they claim.
# ~3 s, and the leviculum-std test deps are already built by `mvr`, so it is
# nearly free here. No tier ran this suite before — `fast` runs `--lib` only
# and `standard` names leviculum-std suites one by one — which is the
# Guarantee-B-masking-C shape the concept page warns about.
citation-guard:
    {{manifest}} citation-guard -- cargo test -p leviculum-std --test doc_citations -- --nocapture

# Codeberg #205: no commit since the pinned baseline may carry a machine-
# authorship trailer. ~20 ms — one `git log` over the range and one `awk`.
#
# The enforcement is the forge check (.woodpecker/commit-trailers.yml), which
# a fresh clone cannot skip. This entry is what puts the same check on the
# pre-push path, where `.githooks/pre-push` runs `just fast`: the incident
# this exists for was a trailer that reached a commit and was caught by a
# human reading the message in the window between committing and pushing.
check-trailers:
    @bash scripts/check-commit-trailers.sh

# Codeberg #310: exactly one list of the binaries periculum mounts, and it is
# `build-integ-bins` below. A caller that writes its own `cargo build --bin`
# line has copied that list, and the copy drifts unseen until a hardware
# nightly aborts in its freshness preflight naming a binary nothing built.
# ~30 ms, reads two files, and self-tests its classifier on seven fixtures
# first. Same family, same reasons, as check-submodules above.
check-integ-bin-list:
    @bash scripts/check-integ-bin-list.sh

# Every long-lived process spawn goes through
# `leviculum_std::process::spawn_supervised`, so the kernel takes the child down
# with its parent however the parent dies. ~200 ms, no build: it reads the
# sources. Bare `Command::new(..).spawn()` sites are counted per file against
# scripts/supervised-spawn-counts.txt, so a new one is a diff rather than seven
# orphaned daemons found by hand four hours later (2026-08-07).
#
# In `fast` because it passes all three hook conditions: fast, deterministic
# given the tree, and it fails naming a file:line the author has open. The
# behaviour itself is proved by `--test supervised_spawn` below; this is the
# check that the proof still covers every site.
check-supervised-spawns:
    @python3 scripts/check-supervised-spawns.py

# Regenerate THIRD-PARTY-NOTICES from the two lockfiles (Codeberg #288).
# Needs cargo-about; scripts/install-ci.sh installs the pinned version.
notices:
    @python3 scripts/gen-notices.py

# Codeberg #288: every published artifact carried our AGPL LICENSE and no
# notice for the MIT- and BSD-licensed crates statically linked into it, which
# both families require to accompany a BINARY distribution. The notices are
# generated from Cargo.lock, checked in, and copied into the .debs, the
# userspace tarballs and the lnflash bundle. This is what stops the checked-in
# copy from describing a dependency graph that no longer exists.
#
# In `fast` rather than in the nightly's `ci-gate`, on all three of the
# conditions the gates above are held to. Fast: ~20 s, no compilation — it
# reads Cargo.lock and the licence files already in the cargo cache. Deterministic
# given the tree: `--frozen` throughout, so no network and no lockfile update
# can move the output, and a CRLF licence file cannot either (the generator
# normalises line endings; without that the guard failed against a file it had
# just written itself). And it fails naming the one command that fixes it, in
# the same session that added the dependency.
#
# The placement follows from where the file can rot: a `cargo add` is the
# moment the checked-in list stops matching, and the push path is the last
# point at which the person who typed it is still there. `ci-gate` would catch
# it too, but a night later and against a container that has neither the
# firmware workspace fetched nor cargo-about installed.
notices-guard:
    @python3 scripts/gen-notices.py --check

# The kernel-enforced half of "a harness that spawns a long-lived process must
# ensure it dies with the harness" (docs/src/concepts/checks-and-citations.md).
# ~10 s: it SIGKILLs a parent and watches its child disappear, plus the negative
# control where the same child — spawned without the fix — must survive. Both
# arms are bounded and fail loudly rather than waiting, which is the mistake the
# incident behind them was about.
supervised-spawn:
    {{manifest}} supervised-spawn -- cargo test -p leviculum-std --test supervised_spawn

# Codeberg #220: `just fast` was green on a tree where eight integration-test
# targets did not compile. Every Tier-0 gate builds workspace libs only, so an
# E0616 in tests/ (a field privatised under a test that reads it) rode the
# pre-push hook unseen and was first caught by `cargo test --workspace`.
# Compile-only, no execution: every lib, bin, example and integration-test
# target must build. Measured 2026-08-12 on a warm tree: ~11 s after a batch,
# ~0.2 s as a no-op — cheap enough for the push path. `--no-tests` declares
# the empty manifest as intended, so the wrapper's executed-zero failure keeps
# guarding the gates that do run tests.
check-all-targets:
    {{manifest}} check-all-targets --no-tests -- cargo check --workspace --all-targets

# Tier 0 (~3.5 min, runs on every git push): submodule pins + commit-message
# trailers + the single-integ-bin-list guard (#310)
# + fmt + clippy (host + nrf) + rustdoc gate + tracing-shim + M0
# gates + a compile check of every workspace target (#220) + workspace lib
# tests + the core suite on a 32-bit `usize` (#303) + the citation guard +
# the third-party notice guard (#288) + the process-supervision pair (census
# over the sources, proof against the kernel).
#
# notices-guard sits after lint-nrf deliberately: it reads the firmware
# workspace `--frozen`, and lint-nrf is what guarantees that workspace's git
# dependencies are fetched by the time it runs.
#
# `clippy --all-targets`, matching ci-gate below. Without it this line lints
# libs and bins only, so a lint that fires solely in test code is invisible on
# the push path: `clippy --workspace --all-targets` was red for three days in
# August 2026 (an `assertions_on_constants` in a #349 test, fixed in c746bf8)
# while every per-batch and pre-push run of this recipe stayed green. The
# `check-all-targets` dependency compiles those targets but does not lint
# them, which is exactly the gap.
fast: check-submodules check-trailers check-integ-bin-list check-supervised-spawns check-processor-seam mvr supervised-spawn lint-nrf nrf-stack-frames nrf-evt-max-size nrf-gap-device-name nrf-board-pins nrf-sd-guard nrf-uf2-volumes nrf-fw-readback nrf-shellcheck hw-witness notices-guard doc-gate core-no-tracing m0-build-gate lxmf-embedded-gate i686-usize-gate check-all-targets citation-guard
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    {{manifest}} workspace-lib -- cargo test --workspace --lib

# The gate .woodpecker/nightly.yml runs before it builds anything it publishes
# (Codeberg #266). Until it existed, nothing between a commit landing on master
# and a .deb appearing on the public releases page executed a single test: the
# pre-push hook is per-clone local config, and `--no-verify` skips it.
#
# NOT an alias for `fast`, because `fast` cannot run in that pipeline's
# container, and not for want of a package:
#   check-submodules      — the pipeline clones with `submodules: false`
#                           (nightly.yml:96-101), so every pin is "missing".
#   lint-nrf              — leviculum-nrf is its own workspace, needs the
#                           thumbv7em target plus flip-link as its linker.
#   nrf-stack-frames      — reads a linked firmware ELF that is never built here.
#   nrf-evt-max-size      — resolves the firmware workspace's feature graph
#                           `--frozen`, so it needs that workspace's git
#                           dependencies already fetched.
#   m0-build-gate,
#   lxmf-embedded-gate    — thumbv6m / thumbv7em cross-compiles.
# Those keep running on the push path, which has the targets and the submodules.
# What is left is what a submodule-less host-target container can actually
# prove, and it is the majority of the suite: fmt, clippy, a compile check of
# every workspace target (#220 — `--lib` gates were green on a tree where eight
# integration-test targets did not build), and the ~3050 workspace lib tests.
#
# It is a recipe rather than four lines of YAML for the reason nightly.yml
# records at :157-160 for the .deb build: a second copy in the pipeline file
# drifts from the gate developers run, and the drift is found the same way.
#
# Measured cold (fresh rust:bookworm, empty target dir and cargo registry,
# 4 cores) on 2026-08-18: 2m12s for the four lines it had then, 3051 tests
# executed across 10 units. The step's provisioning — musl-tools, the rustfmt
# and clippy components, `cargo install just`, one shallow submodule — costs
# 1m11s on top, so the pipeline pays 3m23s to stop shipping untested .debs.
# Re-measured after the widening below, on schneckenschreck with an empty
# target dir but a warm cargo registry: 1m58s, 3057 tests across 10 units.
#
# `clippy --all-targets` and no separate `cargo check`: clippy compiles what
# check compiles, so the check line was a second pass over the same targets.
# The lint findings that kept clippy off test code until 2026-08-18 are fixed.
ci-gate:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    {{manifest}} ci-gate-workspace-lib -- cargo test --workspace --lib

# First run after a fresh CARGO_TARGET_DIR: 20-40 min. Nothing triggers this
# automatically: it is typed once per batch. A post-commit hook detached it
# after every commit until 2026-08-07 — a commit is not a unit anyone wants
# tested, and forty minutes is not a wait a commit can absorb. See
# docs/src/concepts/checks-and-citations.md.
# Tier 1 (~15 min): Tier 0 + core/tests + ffi (incl. C-program + Python interop)
# + proxy + rnsd_interop + the TCP-hub endurance smoke soak.
#
# Note on `--tests` targets: Tier 0 runs `--lib` only, so a crate's
# tests/ directory is covered here or nowhere. lblogd was in the "nowhere"
# bucket until 2026-07-27 (its node_integ end-to-end test never ran in CI).
# lnomad/tests/ and leviculum-micron/tests/ are still uncovered, as are
# several leviculum-std suites beyond the three named below.
standard: fast test-ffi verify-packaging
    {{manifest}} core-tests -- cargo test -p leviculum-core --tests
    # ~4 s: node_integ builds an in-process daemon + IPC + blog node, which
    # is too slow for the Tier 0 push gate but trivial here.
    {{manifest}} lblogd-tests -- cargo test -p lblogd --tests
    {{manifest}} proxy -- cargo test -p leviculum-proxy
    {{manifest}} rnsd-interop -- cargo test -p leviculum-std --test rnsd_interop
    {{manifest}} event-log-subscriber -- cargo test -p leviculum-std --test event_log_subscriber -- --test-threads=1
    {{manifest}} event-log-multiprocess -- cargo test -p leviculum-std --test event_log_multiprocess
    # The LNode debug-log format contract (Codeberg #65): pins the [HEAP] and
    # [PANIC_COUNT] line shapes that catch-reboot.sh and the by-hand heap
    # analysis grep, against the firmware source that emits them.
    {{manifest}} lnode-debug-log-format -- cargo test -p leviculum-std --test lnode_debug_log_format
    # The LXMF helper end to end (#196): two `lxmf-node` processors over TCP
    # loopback exchange messages in both directions, plus the partitioned
    # negative control. ~11 s, no docker — this is the part of the helper's
    # evidence that does not need the rig, and Tier 0 runs `--lib` only, so a
    # crate's tests/ directory is covered here or nowhere.
    {{manifest}} lxmf-node -- cargo test -p leviculum-lxmf-node --test two_node_loopback
    # Endurance gate (#101): builds lnsd, boots it as a hub, asserts 100%
    # delivery + RSS plateau + no fd leak. ~15 s smoke; `--full` is on demand.
    bash scripts/run-soak.sh
    # #191a: the only suite that drives lnsd AND the vendor Python rnsd
    # through one traffic script. ~196 s. Kept #[ignore]d and run by name
    # (serially, apart from the parallel suite) — see the script header.
    bash scripts/run-status-parity.sh
    # #191c: the #[ignore]d bucket is a pinned number per test unit, so it
    # cannot grow without a diff. Last, because it needs every workspace
    # test binary built and this tier has built most of them already.
    python3 scripts/check-ignored-counts.py

# Build the production binaries periculum mounts into its node containers. Explicit per-bin list avoids `--workspace --bins` which
# would also try to build leviculum-nrf firmware on the host. Runs on
# the same CARGO_TARGET_DIR as the enclosing `cargo test`, so the
# runner's CARGO_TARGET_DIR-aware path resolver finds them.
#
# Touch the bin-crate sources first so cargo always relinks and stamps a
# fresh mtime: after a repo-sync pulls newer commits without changing
# source mtimes, cargo would otherwise skip the relink and leave
# binaries that check_binary_freshness rejects (2026-06-13 nightly).
# Every tier that mounts binaries depends on this target, so the
# guarantee holds for tier1/tier2 the same way run-tier3-hw.sh enforces
# it for the hardware nightly. Deleting the binary does NOT work: cargo
# re-hardlinks it from deps/ without relinking, keeping the old mtime.
build-integ-bins:
    find leviculum-cli/src leviculum-proxy/src leviculum-lxmf-node/src -name '*.rs' -exec touch {} +
    cargo build --release --bin lnsd --bin lnstest --bin lncp --bin lnstatus --bin lora-proxy --bin lxmf-node

# Tier 2 (~30-90 min, on demand: `systemctl --user start
# leviculum-ci-tier2.service`): Tier 1 + the docker scenario suites.
#
# Runs periculum over its two tier-2 corpora: `conformance/` (portable
# shared-property scenarios that must be all green against any pair of
# implementations) and `regression/` (leviculum-specific product scenarios,
# IP-based, excluded from conformance by its admission rules). Both bind no
# physical device, so this tier needs docker and nothing else. `hardware/` is
# tier 3 and is not run here — periculum would report every one of its
# scenarios SKIPPED_INFRA, which is honest but says nothing.
#
# Exit-code contract: 0 = something ran and nothing was RED, 1 = at least one
# RED, 2 = usage/malformed/internal, 3 = nothing ran at all. The bare
# invocation therefore fails the recipe on 1, 2 and 3, which is what a tier
# gate wants.
# The run that covers everything BY CONSTRUCTION (Guarantee B,
# docs/src/concepts/checks-and-citations.md). Tiers define latency, not
# coverage: `fast` runs `--lib`, `standard` names packages one at a time, and
# 327 ordinary tests were consequently executed by no gate at all (Codeberg
# #194) — whole crates, not stragglers. Naming the missing ones is what
# produced the gap in the first place and loses something again with every new
# test file, so this selects nothing by name. Leaving a test out of the
# complete run is now what has to be declared.
#
# Two commands, because neither spelling covers everything alone. `--all-targets`
# runs every lib, bin and integration target and additionally compiles the six
# leviculum-std examples — but cargo DROPS doctests when it is given, and
# scripts/ignored-counts.txt tracks two doctest units, so `--doc` is its own
# invocation rather than a footnote. The workspace has no bench targets
# (`cargo metadata`, checked 2026-08-06), so `--all-targets` pulls in nothing
# that fails to compile.
#
# `--no-fail-fast` is load-bearing for the manifest, not a convenience: without
# it cargo stops after the first red binary and every later binary goes unrun,
# so a red run would emit a manifest covering a prefix of the workspace while
# looking like the whole of it.
#
# No `--test-threads=1`. It was needed for port contention, which the host-wide
# allocator (leviculum-std/tests/support/port_alloc.rs) removed, and for the
# event-log subscriber suite, which now takes its own lock. Measured green at
# default parallelism, 3669 tests + 28 doctests, 2026-08-06.
#
# Wall time on this host (4 cores, warm target dir): 7m50s + 3s. A fresh
# CARGO_TARGET_DIR adds a full workspace test build on top — which is why this
# one gate raises the wrapper's 1800 s default. 493 s is the longest run the
# manifests have recorded for it, so the default is comfortable warm and only
# marginal cold, and a backstop that fires on an honest cold build is a backstop
# people switch off. The nightly's `timeout 3600` around all of `just complete`
# stays the outer bound; this is the inner one, so an interactive cold run is
# not cut off mid-build.
complete:
    {{manifest}} workspace-all-targets --timeout 3600 -- cargo test --workspace --all-targets --no-fail-fast
    {{manifest}} workspace-doc -- cargo test --workspace --doc --no-fail-fast

extensive: standard complete build-integ-bins build-c-lnsd
    #!/usr/bin/env bash
    set -euo pipefail
    PERICULUM_ROOT="${PERICULUM_ROOT:-../periculum}"
    PERICULUM_BIN="${PERICULUM_BIN:-$PERICULUM_ROOT/target/release/periculum}"
    if [ ! -x "$PERICULUM_BIN" ]; then
        echo "[extensive] periculum binary missing - building in $PERICULUM_ROOT"
        # Pin the target dir so the binary lands where the PERICULUM_BIN
        # default expects it, even when the CI runner exports a global
        # CARGO_TARGET_DIR (run-tier2.sh does).
        (cd "$PERICULUM_ROOT" && CARGO_TARGET_DIR=target cargo build --release)
    fi
    "$PERICULUM_BIN" run "$PERICULUM_ROOT/conformance" "$PERICULUM_ROOT/regression"

# Tier 3 (~2-6h, 02:00 nightly): Tier 2 + the LoRa hardware corpus.
#
# Two steps periculum does not do itself. First the LNodes are flashed from
# HEAD and their [FW_BUILD] banner is read back, because periculum tests
# whatever firmware it finds and leaves board preparation out of scope on
# purpose; a run against stale firmware is meaningless, so an unverifiable
# board fails the tier while still letting the rest of the corpus run. Then
# periculum runs `hardware/`, whose scenarios bind a modem or a firmware node
# and report SKIPPED_INFRA — never RED — for any board this bench does not
# hold.
#
# The scheduled nightly goes through scripts/run-tier3-hw.sh instead, which
# adds the CI ledger, the repo sync and the USB device-vanish watchdog.
nightly: extensive
    #!/usr/bin/env bash
    set -euo pipefail
    PERICULUM_ROOT="${PERICULUM_ROOT:-../periculum}"
    PERICULUM_BIN="${PERICULUM_BIN:-$PERICULUM_ROOT/target/release/periculum}"
    unverified=$(bash scripts/flash-lnodes-from-head.sh | awk '$1 == "FW_UNVERIFIED" { print $2 }' | paste -sd, -)
    rc=0
    "$PERICULUM_BIN" run "$PERICULUM_ROOT/hardware" || rc=$?
    if [ -n "$unverified" ]; then
        echo "[nightly] FIRMWARE UNVERIFIED: LNode(s) $unverified could not be confirmed to run HEAD" >&2
        echo "[nightly] the run tested UNKNOWN firmware on those boards" >&2
        rc=1
    fi
    exit "$rc"

# Build leviculum-ffi as a real glibc-dynamic cdylib + staticlib for
# C-API consumers ("apt install libreticulum-dev" ergonomics). This
# deliberately overrides the workspace musl default — see the comment
# in .cargo/config.toml. cbindgen regenerates leviculum.h as a side
# effect of the build.rs.
# Comprehensive C API test suite on the glibc target: the Rust unit,
# integration, and Python-interop suites plus the C acceptance programs linked
# against the real cdylib. Builds the debug glibc cdylib first, because once
# the crate has an rlib `cargo test` no longer builds the cdylib, and the
# C-program harness needs libleviculum.so to link and run. The Python interop
# tests skip cleanly if Python RNS is unavailable.
test-ffi:
    cargo build -p leviculum-ffi --target x86_64-unknown-linux-gnu
    {{manifest}} ffi -- cargo test-ffi

# Memory- and race-check the C API under sanitizers and Miri. On demand, not in
# the standard tiers: it needs the nightly toolchain
# (`rustup toolchain install nightly --component rust-src miri`) and is heavy,
# since -Zbuild-std rebuilds std and every dependency with instrumentation
# (several GB of target per sanitizer). AddressSanitizer (+ LeakSanitizer) and
# ThreadSanitizer run the in-process two-node integration suite, covering the
# handle lifecycle, the eventfd bridge, and the two-runtime threading; ASan also
# runs the property suite, where randomised buffer sizes stress the read(2)
# protocol for overflows. Miri
# checks the pure unsafe marshalling paths (buffer read(2), handle boxing,
# char** aspects); it cannot run tokio or real I/O, so node/network tests are
# excluded by filtering to identity/hex/destination.
sanitize-ffi:
    RUSTFLAGS="-Zsanitizer=address" cargo +nightly test -p leviculum-ffi -Zbuild-std --target x86_64-unknown-linux-gnu --test ffi_unit --test ffi_integration --test ffi_property -- --test-threads=1
    RUSTFLAGS="-Zsanitizer=thread" TSAN_OPTIONS="halt_on_error=0 suppressions={{justfile_directory()}}/leviculum-ffi/tsan-suppressions.txt" cargo +nightly test -p leviculum-ffi -Zbuild-std --target x86_64-unknown-linux-gnu --test ffi_integration -- --test-threads=1
    MIRIFLAGS="-Zmiri-disable-isolation" cargo +nightly miri test -p leviculum-ffi --test ffi_unit identity
    MIRIFLAGS="-Zmiri-disable-isolation" cargo +nightly miri test -p leviculum-ffi --test ffi_unit hex
    MIRIFLAGS="-Zmiri-disable-isolation" cargo +nightly miri test -p leviculum-ffi --test ffi_unit destination

build-ffi:
    cargo build-ffi

# Verify libleviculum installs and links like a standard Unix C library: a
# staged `make install` produces the SONAME symlink chain, header, static
# archive, and pkg-config file, and a consumer compiles, links, and runs
# against it purely through pkg-config, both dynamically and statically.
# Catches a renamed export breaking the header, a wrong .pc, a missing soname,
# or a load failure. Part of Tier 1.
verify-packaging:
    bash scripts/verify-packaging.sh

# Same end-to-end packaging check for the aarch64 cross build: cross-compiles
# the consumer and runs it under qemu. Skips cleanly if the cross toolchain
# (rustup target + gcc-aarch64-linux-gnu + qemu-user-static) is absent.
verify-packaging-arm64:
    bash scripts/verify-packaging.sh aarch64-unknown-linux-gnu

# Same for ARM64. Requires `sudo apt install gcc-aarch64-linux-gnu` and
# `rustup target add aarch64-unknown-linux-gnu` on the build host.
build-ffi-arm64:
    cargo build-ffi-arm64

# Build the C daemon (examples/c/lnsd.c) as a self-contained binary, linking
# libleviculum.a statically (glibc stays dynamic, matching the debian-slim
# node container). Output: target/release/c-lnsd, the binary periculum mounts
# for a node whose adapter is `c-lnsd`.
build-c-lnsd: build-ffi
    T="${CARGO_TARGET_DIR:-target}"; \
    mkdir -p "$T/release"; \
    cc leviculum-ffi/examples/c/lnsd.c \
       "$T/x86_64-unknown-linux-gnu/release/libleviculum.a" \
       -I leviculum-ffi -O2 -Wall -Wextra -Werror \
       -lpthread -ldl -lm \
       -o "$T/release/c-lnsd"

# Local .deb production, mirroring .woodpecker/nightly.yml (build-amd64 +
# build-arm64). Use to build a master .deb by hand for the aarch64 soak
# node (miauhaus) without waiting for CI. The nightly pipeline is the
# source of truth; these recipes replicate its exact steps minus the
# publish/upload (that stays CI-only). Tooling: rustup targets
# x86_64/aarch64-unknown-linux-musl, cargo-deb, and for arm64
# cargo-zigbuild + ziglang. Run `just _deb-prereqs` to install them.

# Pin the build ID + per-package DEB versions once and persist them, so
# an amd64 + arm64 pair from a single `just build-deb` run carries
# identical version strings (no midnight-UTC drift between the two
# builds). Shared with nightly.yml, which calls the same script: this
# used to be duplicated shell in both places and the two drifted.
_deb-stamp:
    @bash scripts/deb-stamp.sh

# amd64 musl-static .debs for all three packages: leviculum (the daemon
# and its clients), lnomad (the browser), lblogd (the blog server).
# Binaries come from the workspace musl target, so they are fully static
# and run on Debian >= 9 / Ubuntu >= 16.04 regardless of host glibc.
#
# The build itself lives in scripts/build-deb.sh, which nightly.yml calls
# too: the sequence used to be written out in both places and drifted,
# stalling the nightly release for eight days (see the script's header).
# Output: target/debian/*_amd64.deb, hardlinked by cargo-deb under
# target/<triple>/debian/ as well.
build-deb-amd64: (_require-cargo-deb) _deb-stamp
    @bash scripts/build-deb.sh amd64

# arm64 musl-static .debs via cargo-zigbuild (Zig as the cross
# compiler/linker — the only way to reach aarch64-musl from an amd64 host
# without docker-in-docker or an arm64 runner). Requires cargo-zigbuild +
# ziglang on PATH; `pip install ziglang` provides a self-contained Zig
# the zigbuild wrapper finds, or install a full Zig distribution (the
# bare zig binary without its sibling lib/ fails at `zig cc` with "unable
# to find zig installation directory").
build-deb-arm64: (_require-cargo-deb) _deb-stamp
    @bash scripts/build-deb.sh arm64

# Build every .deb in one go. _deb-stamp runs first (a dependency of each
# child), so all six packages share one build-id and a consistent set of
# per-package versions.
build-deb: build-deb-amd64 build-deb-arm64

# Structural check on the built .debs: metadata, per-package versions,
# file layout, conffiles, unit validity, maintainer-script syntax. Needs
# `just build-deb` (or at least build-deb-amd64) to have run. Root not
# required — nothing is installed.
#
# Deliberately not part of any test tier: it presupposes a build-deb run,
# which does a `cargo clean` on three crates and cross-builds for two
# targets. That is minutes of rebuild plus a zig toolchain, which does
# not belong in the 15-minute Tier 1 budget. Run it by hand whenever
# packaging changes, together with a real install test in a systemd
# container — the structural checks here cannot see a service that
# installs cleanly and then fails to start.
verify-deb:
    @bash scripts/verify-deb-packaging.sh

_require-cargo-deb:
    @cargo deb --version >/dev/null 2>&1 || (echo "cargo-deb not found — run: just _deb-prereqs (or cargo install cargo-deb)" && exit 1)

# Best-effort, idempotent install of the cross-build toolchain the
# build-deb* recipes need: the two musl rustup targets, cargo-deb, and
# cargo-zigbuild + ziglang for the arm64 cross-link. Safe to re-run.
_deb-prereqs:
    rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
    cargo install --locked cargo-deb
    cargo install --locked cargo-zigbuild
    @echo "[_deb-prereqs] also ensure ziglang is available for arm64:"
    @echo "    pip install ziglang   (or install a full Zig distribution on PATH)"

# Status of last runs across all tiers
status:
    @bash scripts/ci-status.sh

# For other tiers: ls ~/.local/state/leviculum-ci/ and pick a file.
# Tail the most recent Tier 1 log (live if a run is in progress).
logs:
    @bash -c 'LOG=$(ls -t ~/.local/state/leviculum-ci/tier1-*.log 2>/dev/null | head -1); \
        if [ -z "$LOG" ]; then echo "No Tier 1 log yet."; exit 1; fi; \
        echo "==> $LOG"; tail -f "$LOG"'

# Idempotent; safe to re-run after pulling.
# Install git hooks and systemd user timers for the 4-tier CI pipeline.
install-ci:
    bash scripts/install-ci.sh

# Touch-free; double-tap RESET only if the runner prompts for a crashed
# device. Details: leviculum-nrf/README.md §Build and flash.
# The runner refuses a board whose SoftDevice our image is not linked for,
# because that write is a soft brick: docs/src/concepts/lnode-flashing.md.
# The firmware crate is outside the workspace (cross-compiled), so we
# invoke cargo from its own directory.
# Flash every attached T114 with the current firmware.
flash:
    cd leviculum-nrf && cargo run --release --bin t114 --features bsp-t114

# SWD firmware debugging via the RPi Debug Probe (see docs/src/firmware/probe-debugging.md)
probe *args:
    ./scripts/probe-debug.sh {{args}}

# Useful for A/B testing (one T114 on new firmware, one on old).
#   just flash-one /dev/ttyACM3
#   just flash-one /dev/leviculum-transport
# Flash a single T114 by port path or udev symlink.
flash-one PORT:
    cd leviculum-nrf && LEVICULUM_FLASH_ONLY={{PORT}} cargo run --release --bin t114 --features bsp-t114

# What every RAK4631 flash recipe tells the runner about this board: the USB
# PID our firmware enumerates on, the names it puts in its messages, and the
# one line that makes the manual double-tap prompt something a person can act
# on. The Pocket V2 has no externally accessible RESET, so the generic
# "double-tap RESET" sends its owner looking for a button that is not there
# (Codeberg #261). Declared once because three recipes share it and a hint
# that drifts between them is worse than none.
rak4631_env := 'LEVICULUM_USB_PID=0002 LEVICULUM_BOARD_NAME=RAK4631 LEVICULUM_UF2_BOARD_ID=WisBlock-RAK4631-Board LEVICULUM_DOUBLE_TAP_HINT="No RESET button on this case: double-tap the reset contact in the hidden pinhole beside the USB socket, with a needle (docs/src/firmware/recovery.md)."'

# First flash from Meshtastic / blank firmware needs a manual RESET
# double-tap (the stock app has no 1200-baud-touch handler). Subsequent
# flashes use the touch path automatically.
# Flash every attached RAK4631 (WisMesh Pocket V2) with the current firmware.
flash-rak4631:
    cd leviculum-nrf && {{rak4631_env}} cargo run --release --bin rak4631 --features bsp-rak4631

# Flash a single RAK4631 by port path or udev symlink.
#   just flash-rak4631-one /dev/ttyACM0
#   just flash-rak4631-one /dev/leviculum-rak-transport
flash-rak4631-one PORT:
    cd leviculum-nrf && LEVICULUM_FLASH_ONLY={{PORT}} {{rak4631_env}} cargo run --release --bin rak4631 --features bsp-rak4631

# Flash with all RAK19026 baseboard peripherals enabled — the WisMesh
# Pocket V2 build. `--features rak-baseboard` aggregates the three
# baseboard features (display, gnss, battery). This is the build the lnflash
# bundle ships for this board (docs/src/concepts/board-support-scope.md).
flash-rak4631-pocket:
    cd leviculum-nrf && {{rak4631_env}} cargo run --release --bin rak4631 --features bsp-rak4631,rak-baseboard

# Trigger Adafruit-UF2-bootloader on a stock-Meshtastic WisMesh Pocket V2.
# Stock Meshtastic has no 1200-bps-touch handler and the device has no
# externally accessible RESET pin, so the firmware-side admin command is the
# only software-only DFU entry. After our firmware lands, just-flash-rak4631
# uses the touch handler from src/usb.rs and this recipe is no longer needed.
# Requires the meshtastic CLI on PATH (pip install meshtastic).
# Usage: just dfu-rak4631 /dev/ttyACM0
dfu-rak4631 PORT:
    meshtastic --port {{PORT}} --enter-dfu

# RNode (LilyGO T-Beam, ESP32 + SX1276) flashing with Mark's firmware.
# Run on the host the RNodes are attached to. The ESP32 has a mask-ROM
# download bootloader and cannot be bricked: a failed flash is always
# recoverable by re-running flash-rnode. This is unlike the nRF52 LNodes
# (T114, RAK4631), where a bad external image leaves the device USB-dark.
#
# Run flash-rnode-setup once first. It pip-installs esptool into a
# gitignored repo-local venv (.rnode-tools/): the Debian esptool package is
# dfsg-stripped of its flasher stubs and fails on large flash reads/writes,
# so a real esptool is needed. rnodeconf is the repo's vendored copy.
# Mark's autoinstall is interactive (product menu); instead we read Mark's
# signed firmware images off a known-good RNode once (flash-rnode-extract,
# into the gitignored .rnode-fw/), then write them back. The write covers
# only the firmware regions, not the NVS/EEPROM partition, so the device
# signature and provisioning are preserved (verified: a T-Beam stayed
# "Validated, Local signature" across a full reflash).

reference_reticulum := justfile_directory() / "reference" / "Reticulum"
rnodeconf := "PYTHONPATH=" + reference_reticulum + " python3 " + reference_reticulum / "RNS" / "Utilities" / "rnodeconf.py"
esptool := justfile_directory() / ".rnode-tools" / "bin" / "esptool.py"
rnode_fw := justfile_directory() / ".rnode-fw"

# One-time setup: a repo-local venv with a working esptool (the Debian
# package cannot read/write large flash regions, its stubs are dfsg-stripped).
flash-rnode-setup:
    python3 -m venv {{justfile_directory()}}/.rnode-tools
    {{justfile_directory()}}/.rnode-tools/bin/pip install --quiet 'esptool<5'

# Read-only device info: connectivity, firmware version, signature.
#   just flash-rnode-info /dev/ttyACM6
flash-rnode-info PORT:
    {{rnodeconf}} --info {{PORT}}

# Back up an RNode EEPROM (board model, signature, provisioning) before any
# flash. Writes ~/.config/rnodeconf/eeprom<timestamp>.eeprom.
flash-rnode-backup PORT:
    {{rnodeconf}} --eeprom-backup {{PORT}}

# Extract Mark's signed firmware images off a known-good, signature-validated
# RNode into .rnode-fw/ (gitignored). Run ONCE against a trusted device; the
# images then serve as the flash source for flash-rnode.
#   just flash-rnode-extract /dev/ttyACM6
flash-rnode-extract PORT:
    mkdir -p {{rnode_fw}}
    {{esptool}} --chip esp32 --port {{PORT}} --baud 921600 read_flash 0x1000 0x4650 {{rnode_fw}}/bootloader.bin
    {{esptool}} --chip esp32 --port {{PORT}} --baud 921600 read_flash 0x8000 0xc00 {{rnode_fw}}/partitions.bin
    {{esptool}} --chip esp32 --port {{PORT}} --baud 921600 read_flash 0xe000 0x2000 {{rnode_fw}}/boot_app0.bin
    {{esptool}} --chip esp32 --port {{PORT}} --baud 921600 read_flash 0x10000 0x200000 {{rnode_fw}}/app.bin
    {{esptool}} --chip esp32 --port {{PORT}} --baud 921600 read_flash 0x210000 0x1f0000 {{rnode_fw}}/console.bin

# Flash a T-Beam RNode with the extracted Mark firmware. Deterministic and
# non-interactive. Preserves the EEPROM provisioning. Requires
# flash-rnode-extract to have populated .rnode-fw/ first.
#   just flash-rnode /dev/ttyACM6
flash-rnode PORT:
    {{esptool}} --chip esp32 --port {{PORT}} --baud 921600 --before default_reset --after hard_reset write_flash --flash_mode dio --flash_freq 80m --flash_size detect 0x1000 {{rnode_fw}}/bootloader.bin 0x8000 {{rnode_fw}}/partitions.bin 0xe000 {{rnode_fw}}/boot_app0.bin 0x10000 {{rnode_fw}}/app.bin 0x210000 {{rnode_fw}}/console.bin
