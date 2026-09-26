# Licensing and third-party notices

Leviculum is licensed under the GNU Affero General Public License,
version 3 or later. That is the licence of the work, it is what `LICENSE`
at the root of the repository contains, and everything below is additive
to it rather than a qualification of it.

## Why a second file exists

Our binaries are musl-static by default. A statically linked binary does
not merely sit next to its dependencies, it contains them, and a large
part of the Rust ecosystem we depend on is MIT or BSD-3-Clause. Both
licence families require their copyright line and their permission text
to accompany a **binary** distribution, not only a source one. Apache-2.0
§4 says the same thing in more words.

Until Codeberg #288 the only licence text in any published artifact was
our own AGPL. Every `.deb`, every userspace tarball and the lnflash
bundle were therefore short a notice for each such crate — unintentional,
and awkward to repair after publication because published artifacts stay
published.

`THIRD-PARTY-NOTICES` at the repository root is that notice. It ships
inside every artifact:

| artifact | path |
| --- | --- |
| `leviculum`, `lnomad`, `lblogd` `.deb` | `/usr/share/doc/<pkg>/THIRD-PARTY-NOTICES` |
| userspace tarballs | `doc/THIRD-PARTY-NOTICES` |
| lnflash bundle | `THIRD-PARTY-NOTICES` beside the binary |
| source tarball | tracked file, included by `git archive` |

`cargo-deb` does write a `copyright` file of its own, but it is derived
from our `Cargo.toml` metadata and describes our code alone, so its
presence never closed this.

## How it is produced

Generated, never hand-curated: a list maintained by hand drifts from
`Cargo.lock` the first time somebody runs `cargo add`.

`scripts/gen-notices.py` drives [`cargo-about`], pinned at 0.9.2 by
`scripts/install-ci.sh`. cargo-about reads each crate's own `LICENSE`
files out of the cargo cache, so the copyright lines in the output are
the crates' real ones rather than a template with the names left blank.
The script decides layout and ordering, drops our own AGPL crates in
favour of the leading section, and refuses to emit a file if any crate
was classified without a licence text.

[`cargo-about`]: https://github.com/EmbarkStudios/cargo-about

Two dependency graphs go in, because `leviculum-nrf` is a separate
workspace with its own lockfile and cannot be reached from the root
manifest:

- **Part 1, host binaries** — `lnsd`, `lnstest`, `lncp`, `lnstatus`,
  `lnomad`, `lblogd`, `lnflash`, resolved for both musl triples.
- **Part 2, the LNode firmware image** — the `t114` build the lnflash
  bundle carries, resolved for `thumbv7em-none-eabihf`.

Everything runs `--frozen`. Offline is not only hygiene: with network
access cargo-about falls back to fetching licence files from a crate's
upstream repository, and output that depends on whether the machine had
connectivity cannot be diffed.

Two configuration decisions are worth naming.

**Dual-licensed crates.** The common `MIT OR Apache-2.0` is taken under
Apache-2.0. Either satisfies us; Apache-2.0 is simply explicit about what
a binary distribution must carry, where MIT leaves it to be reconstructed.
The preference is the order of the `accepted` list in `about.toml`.

**Nordic's SoftDevice bindings.** `nrf-softdevice-s140` declares a
`license-file` and no SPDX `license` field, so cargo-about dropped it
with a warning — and, worse, its file crawler had been reading Nordic's
five-clause text as plain BSD-3-Clause, which it is not: clauses 4 and 5
add restrictions BSD does not have. `leviculum-nrf/about.toml` now
clarifies it as `LicenseRef-Nordic-5-Clause` with the licence file's
checksum pinned, and the generator reproduces that text verbatim.

This is distinct from the SoftDevice **blob**, which travels through the
lnflash bundle as a separate `.hex` with its own licence agreement beside
it and is never linked into anything. The bindings are compiled into our
UF2; the blob is not.

## How it is kept honest

`just notices-guard` regenerates the file and diffs it byte for byte
against the checked-in copy. It runs in `just fast`, so it is on the
pre-push path: adding a dependency without regenerating turns the push
red, in the same session that added it.

The file is checked in rather than generated at artifact-build time on
purpose. The `.deb`, tarball and bundle builds then copy a tracked file
and need neither cargo-about nor a network, and the freshness question
lives in one place instead of four build scripts.

To fix a red guard:

```
just notices     # regenerate
git add THIRD-PARTY-NOTICES
```

Two further checks assert the file actually arrives:
`scripts/verify-deb-packaging.sh` looks for it in each `.deb`, and
`scripts/lnflash-bundle.sh` asserts it — and both of its section
headings — inside the finished tarball rather than in the staging
directory, because the tarball is what ships.

## What is not covered

`leviculum-ffi` installs through its own `make install` and is not part
of the nightly publish set, so no notice file is installed alongside
`libleviculum`. A consumer linking the static archive takes on the same
obligation; when that library starts being published as an artifact it
needs the same treatment.
