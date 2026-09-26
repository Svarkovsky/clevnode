# Fuzz harness for the leviculum-std parsers (Codeberg #108)

Coverage-guided fuzzing (cargo-fuzz / libFuzzer) for the leviculum-std
functions that parse UNTRUSTED bytes. Companion to `leviculum-core/fuzz/`
(Codeberg #23); a separate crate because the parser under test lives in
leviculum-std and pulling that into the core fuzz crate would drag the whole
daemon stack into a crate named for core.

A parser that panics, overflows (in debug), infinite-loops, or OOMs on
malformed input is a remote DoS, so every target must return `Err`/`None`
gracefully on any input.

## Requirements

- Rust **nightly**: `rustup toolchain install nightly`
- **cargo-fuzz**: `cargo install cargo-fuzz`
- Build/run against the **glibc** host target, NOT the workspace musl default
  (`.cargo/config.toml` sets musl; ASan wants glibc):
  `--target x86_64-unknown-linux-gnu`

This crate is detached from the repo workspace (its own `[workspace]` table)
and is excluded from `just standard`; regression tests for any crash it finds
live in the normal `leviculum-std` unit suite instead.

## Targets (ranked by exposure)

| target      | parser                                              | reachability |
|-------------|-----------------------------------------------------|--------------|
| `sam_parse` | `interfaces::i2p::sam::Message::parse` + `i2p_b64decode` + `Destination::from_{public,private}_base64` | every reply line from the I2P SAM bridge socket |

`Message::parse` splits a SAM reply line into a command/action verb and
`KEY=VALUE` options; a `DESTINATION=` value is then routed through the I2P
base64 decoder and the destination decoders (which read a big-endian
certificate length and slice the key blob). The base64 decoder does bit-level
shift/accumulate arithmetic on attacker-controlled length, so it is the most
arithmetic-heavy path and is fuzzed both via a `DESTINATION=` option and
directly on the whole input.

## Run a bounded smoke (catch shallow crashes)

```sh
cd leviculum-std
cargo +nightly fuzz run sam_parse --target x86_64-unknown-linux-gnu \
    fuzz/seeds/sam_parse -- -max_total_time=30 -max_len=8192 -rss_limit_mb=2048
```

`fuzz/seeds/sam_parse/` holds a small hand-written seed corpus (committed). The
generated working corpus lands in `fuzz/corpus/sam_parse/` (gitignored), and
any crash is written to `fuzz/artifacts/sam_parse/`.

## Reproduce a specific input

```sh
cargo +nightly fuzz run sam_parse --target x86_64-unknown-linux-gnu <file>
```

## CI / nightly follow-up

The 30 s smoke here catches shallow crashes only. Deep continuous fuzzing
(hours per target, corpus persisted across runs) belongs in a nightly job, not
`just standard`.
