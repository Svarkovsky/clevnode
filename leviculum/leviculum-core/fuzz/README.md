# Fuzz harness for the wire-format parsers (Codeberg #23)

Coverage-guided fuzzing (cargo-fuzz / libFuzzer) for the functions that parse
UNTRUSTED bytes off the wire. A parser that panics, overflows (in debug),
infinite-loops, or OOMs on malformed input is a remote DoS, so every target
must return `Err`/`None` gracefully on any input.

## Requirements

- Rust **nightly** (libFuzzer needs `-Z` sanitizer flags): `rustup toolchain install nightly`
- **cargo-fuzz**: `cargo install cargo-fuzz`
- Build/run against the **glibc** host target, NOT the workspace musl default
  (`.cargo/config.toml` sets musl; ASan wants glibc):
  `--target x86_64-unknown-linux-gnu`

This crate is detached from the repo workspace (its own `[workspace]` table)
and is excluded from `just standard`; the regression tests for any crash it
finds live in the normal `leviculum-core` unit suite instead.

## Targets (ranked by exposure — network-reachable first)

| target                           | parser                                              | reachability |
|----------------------------------|-----------------------------------------------------|--------------|
| `packet_unpack`                  | `packet::Packet::unpack` (+ `packet_hash`)          | every inbound wire packet, every interface |
| `resource_advertisement_unpack`  | `resource::ResourceAdvertisement::unpack`           | peer resource advertisement over an established link, no PoW |
| `discovery_announce`             | `discovery::parse_announce_app_data`                | discovery announce from any peer (PoW-gated; fuzzed with `required_value = 0`) |
| `ifac_verify`                    | `ifac::IfacConfig::verify_ifac` / `has_ifac_flag`   | every inbound packet on an IFAC-guarded interface |
| `hdlc_deframe`                   | `framing::hdlc::Deframer::process`                  | raw serial/TCP byte stream (KISS/HDLC) |
| `kiss_deframe`                   | `framing::kiss::KissDeframer::process`              | raw serial byte stream (RNode/KISS) |

The msgpack readers in `resource/msgpack.rs` (including the recursive
`skip_msgpack_value`) are exercised transitively by
`resource_advertisement_unpack` and `discovery_announce`.

## Run a bounded smoke (catch shallow crashes)

```sh
cd leviculum-core
cargo +nightly fuzz run <target> --target x86_64-unknown-linux-gnu \
    seeds/<target> -- -max_total_time=30 -max_len=8192 -rss_limit_mb=2048
```

`seeds/<target>/` holds a small hand-written seed corpus (committed). The
generated working corpus lands in `corpus/<target>/` (gitignored), and any
crash is written to `artifacts/<target>/`.

## Reproduce a specific input

```sh
cargo +nightly fuzz run <target> --target x86_64-unknown-linux-gnu <file>
```

## Known finding

`resource_advertisement_unpack` originally aborted with an ASan
**stack-overflow** on a deeply nested msgpack container (a chain of
fixarray-len-1 tags routed through `skip_msgpack_value`'s unknown-key skip
path). Fixed by a nesting-depth cap (`MAX_SKIP_DEPTH`) in
`resource/msgpack.rs`; the reproducer is kept at
`seeds/resource_advertisement_unpack/recursion_reproducer` and as unit
regressions (`skip_rejects_deeply_nested_container`,
`test_advertisement_unpack_rejects_deeply_nested_value`).

## CI / nightly follow-up

The 30 s-per-target smoke here catches shallow crashes only. Deep continuous
fuzzing (hours per target, corpus persisted across runs) belongs in a nightly
job, not `just standard`.
