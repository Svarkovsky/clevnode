#!/usr/bin/env python3
"""Generate (or check) THIRD-PARTY-NOTICES from the lockfiles.

Codeberg #288: every published artifact carried our own AGPL text and
nothing else, while MIT- and BSD-licensed crates are statically linked
into all of them. Both licence families require their copyright line and
permission text to accompany *binary* distributions, not only source, so
a .deb or tarball holding LICENSE alone was short a notice per such
crate.

The file is generated, checked in, and shipped verbatim:

    scripts/gen-notices.py            regenerate THIRD-PARTY-NOTICES
    scripts/gen-notices.py --check    regenerate to a temp file and diff

`just notices` and `just notices-guard` are the two entry points. The
guard runs in `just fast`, so a commit that adds a dependency without
regenerating turns the push path red.

Checked in rather than produced at artifact-build time on purpose: the
.deb, tarball and lnflash-bundle builds then need no extra tool and no
network, they copy a tracked file. The freshness question moves to one
place — the guard — instead of being spread across four build scripts.

Two dependency graphs, because leviculum-nrf is its own workspace with
its own Cargo.lock and cannot be reached from the root manifest:

  * the host binaries (lnsd, lnstest, lncp, lnstatus, lnomad, lblogd,
    lnflash), built for the two musl triples;
  * the T114 firmware image, built for thumbv7em-none-eabihf, which
    ships as a UF2 inside the lnflash bundle.

cargo-about does the harvesting (it reads each crate's own LICENSE files
out of the registry cache, so copyright lines are the crates' real ones,
not a template). This script only decides layout and ordering, drops our
own AGPL crates in favour of the leading section, and fails loudly on a
crate whose licence text cargo-about could not produce.

Everything runs `--frozen` (locked + offline). Offline is not just
hygiene: with network access cargo-about falls back to fetching licence
files from a crate's upstream git repo, which would make the output
depend on whether the machine running the guard had connectivity — and
a diff guard over non-deterministic output is worse than no guard.
"""

from __future__ import annotations

import argparse
import difflib
import hashlib
import json
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUTPUT = ROOT / "THIRD-PARTY-NOTICES"

RULE = "=" * 72
THIN = "-" * 72


class Graph:
    """One cargo workspace whose crates end up in a shipped artifact."""

    def __init__(self, key, title, manifest, config, extra_args, feature_args, blurb):
        self.key = key
        self.title = title
        self.manifest = ROOT / manifest
        self.config = ROOT / config
        self.extra_args = extra_args
        # `cargo metadata` prunes optional dependencies that no enabled
        # feature pulls in, so the clarification lookup has to ask for
        # the same feature set the harvest ran with. Without this,
        # nrf-softdevice-s140 is simply absent from the metadata and the
        # lookup silently finds no manifest to read its licence from.
        self.feature_args = feature_args
        self.blurb = blurb


GRAPHS = [
    Graph(
        key="host",
        title="PART 1 — host binaries",
        manifest="Cargo.toml",
        config="about.toml",
        # --workspace rather than one run per published package: the four
        # published crates (leviculum-cli, lnomad, lblogd, lnflash) share
        # nearly their whole graph, and four runs would have to be merged
        # by hand afterwards. The extra members this pulls in
        # (leviculum-proxy, leviculum-lxmf-node, lnmsg, leviculum-ffi) add
        # crates that are not in any published binary today — an
        # over-inclusive notice list, which is harmless, and which stays
        # correct on the day one of them starts shipping.
        extra_args=["--workspace"],
        feature_args=[],
        blurb=(
            "Linked into lnsd, lnstest, lncp, lnstatus, lnomad, lblogd and\n"
            "lnflash. Built for x86_64-unknown-linux-musl and\n"
            "aarch64-unknown-linux-musl."
        ),
    ),
    Graph(
        key="firmware",
        title="PART 2 — LNode firmware image",
        manifest="leviculum-nrf/Cargo.toml",
        config="leviculum-nrf/about.toml",
        # The feature set scripts/lnflash-bundle.sh builds the shipped
        # image with. rak4631 is not published today; when it is, this
        # becomes a second entry rather than an --all-features run, which
        # the mutually exclusive bsp-* features forbid anyway.
        extra_args=["--features", "bsp-t114"],
        feature_args=["--features", "bsp-t114"],
        blurb=(
            "Linked into the t114 UF2 image the lnflash bundle carries.\n"
            "Built for thumbv7em-none-eabihf."
        ),
    ),
]


HEADER = f"""\
{RULE}
THIRD-PARTY NOTICES
{RULE}

Leviculum itself is licensed under the GNU Affero General Public License,
version 3 or later. That licence is the one that governs this software;
its full text travels with every artifact as the file LICENSE, and
nothing below modifies it.

What follows is additive. The binaries in this artifact are statically
linked, so third-party code is inside them rather than beside them, and
several of the licences involved — MIT, the BSD family, Apache-2.0 —
require their copyright notice and permission text to accompany a binary
distribution. This file is how they accompany it.

Crates offering a choice of licences (the common `MIT OR Apache-2.0`)
are taken under Apache-2.0 where it is offered. Both are satisfied by
reproducing them here; Apache-2.0 is simply explicit (§4) about what a
binary distribution must carry, where MIT leaves it to be inferred.

Generated from Cargo.lock by scripts/gen-notices.py — do not edit by
hand. Regenerate with `just notices`.
"""


def normalize(text):
    """One line ending for the whole notice file.

    Crates ship their LICENSE files with whatever their author's editor
    produced, and a fair number are CRLF. Passing those through verbatim
    made the generated file fail its own guard: writing kept the CR
    bytes, reading them back through Python's universal-newline
    translation turned them into LF, and the comparison then found a
    difference that no dependency change had caused. Normalising at the
    point the text enters also merges the CRLF and LF copies of the same
    licence into one section instead of two.
    """
    return text.replace("\r\n", "\n").replace("\r", "\n")


def run(cmd, cwd):
    proc = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.stderr.write(proc.stdout)
        sys.stderr.write(proc.stderr)
        raise SystemExit(f"error: {' '.join(cmd)} failed with {proc.returncode}")
    return proc


def harvest(graph):
    """Run cargo-about over one graph and return its JSON inventory."""
    cmd = [
        "cargo",
        "about",
        "generate",
        "--frozen",
        # Without --fail a crate whose licence expression cannot be read
        # is a warning on stderr and an absence in the output, which is
        # the precise failure this file exists to prevent.
        "--fail",
        "--format",
        "json",
        "-c",
        str(graph.config),
        "-m",
        str(graph.manifest),
        *graph.extra_args,
    ]
    proc = run(cmd, cwd=graph.manifest.parent)
    return json.loads(proc.stdout)


def clarified_text(graph, crate_name, crate_version):
    """Licence text for a crate cargo-about could not produce one for.

    cargo-about has no canonical text for a `LicenseRef-*` licence — by
    definition there is no SPDX corpus entry — and its clarification path
    does not feed the clarified file back into the rendered output, so
    such a crate arrives here classified but textless. Rather than
    hand-copying the text into the repo (a second copy, free to drift),
    read the very file the clarification already names and pins, out of
    the crate's own source in the cargo cache.

    The pinned checksum is what makes this safe: a crate version bump
    that changes the licence text fails the clarification inside
    cargo-about first, so this function never sees an unverified file.
    """
    with graph.config.open("rb") as fh:
        cfg = tomllib.load(fh)
    clarify = cfg.get(crate_name, {}).get("clarify")
    if not clarify:
        return None
    files = clarify.get("files") or []
    if not files:
        return None

    meta = json.loads(
        run(
            ["cargo", "metadata", "--format-version", "1", "--frozen", *graph.feature_args],
            cwd=graph.manifest.parent,
        ).stdout
    )
    manifest_path = None
    for pkg in meta["packages"]:
        if pkg["name"] == crate_name and pkg["version"] == crate_version:
            manifest_path = Path(pkg["manifest_path"])
            break
    if manifest_path is None:
        return None

    chunks = []
    for entry in files:
        path = manifest_path.parent / entry["path"]
        data = path.read_bytes()
        want = entry.get("checksum")
        got = hashlib.sha256(data).hexdigest()
        if want and want != got:
            raise SystemExit(
                f"error: {crate_name} {crate_version}: {entry['path']} is {got}, "
                f"{graph.config.relative_to(ROOT)} pins {want}"
            )
        chunks.append(normalize(data.decode("utf-8")))
    return "\n".join(chunks)


def is_ours(crate):
    """True for a workspace-local crate.

    Path dependencies have a null `source`. Ours are AGPL and their text
    is the leading section of this file plus the LICENSE beside it;
    repeating it once per member would bury the third-party notices.
    Filtered per crate rather than per licence id on purpose — an AGPL
    crate arriving from crates.io must still be listed.
    """
    return crate.get("source") is None


def render(graph, inventory):
    """One section of the notice file, deterministic given the inventory."""
    lines = [RULE, graph.title, RULE, "", graph.blurb, ""]

    # (licence id, exact text) -> crates. cargo-about already splits by
    # text, which is what keeps 90 differently-copyrighted MIT crates
    # from collapsing into one notice that names none of them.
    groups = {}
    for entry in inventory["licenses"]:
        users = sorted(
            {
                (u["crate"]["name"], u["crate"]["version"])
                for u in entry["used_by"]
                if not is_ours(u["crate"])
            }
        )
        if not users:
            continue
        key = (entry["id"], entry["name"], normalize(entry["text"]))
        groups.setdefault(key, set()).update(users)

    # Crates cargo-about classified but produced no text for. Recovered
    # from the clarification, or fatal.
    classified = {
        (c["package"]["name"], c["package"]["version"]): c["license"]
        for c in inventory["crates"]
        if not is_ours(c["package"])
    }
    covered = {u for users in groups.values() for u in users}
    for (name, version), expr in sorted(classified.items()):
        if (name, version) in covered:
            continue
        text = clarified_text(graph, name, version)
        if text is None:
            raise SystemExit(
                f"error: {name} {version} resolves to '{expr}' but no licence "
                f"text was produced for it, and {graph.config.relative_to(ROOT)} "
                f"carries no clarification that supplies one. A shipped binary "
                f"would carry the crate and not its notice."
            )
        groups.setdefault((expr, expr, text), set()).add((name, version))

    ordered = sorted(groups.items(), key=lambda kv: (kv[0][0], sorted(kv[1])[0]))

    total = sorted({u for users in groups.values() for u in users})
    counts = {}
    for (lic_id, _name, _text), users in groups.items():
        counts.setdefault(lic_id, set()).update(users)
    lines.append(f"{len(total)} third-party crates, by licence:")
    lines.append("")
    for lic_id in sorted(counts):
        lines.append(f"  {lic_id:<28} {len(counts[lic_id])}")
    lines.append("")

    for (lic_id, lic_name, text), users in ordered:
        lines.append(THIN)
        lines.append(lic_id if lic_id == lic_name else f"{lic_id} — {lic_name}")
        lines.append(THIN)
        lines.append("")
        lines.append("Applies to:")
        for name, version in sorted(users):
            lines.append(f"  {name} {version}")
        lines.append("")
        lines.append(text.rstrip("\n"))
        lines.append("")

    return "\n".join(lines)


def build():
    parts = [HEADER]
    for graph in GRAPHS:
        parts.append(render(graph, harvest(graph)))
    return "\n".join(parts).rstrip("\n") + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--check",
        action="store_true",
        help="do not write; diff against the checked-in file and exit 1 on drift",
    )
    args = ap.parse_args()

    generated = build()

    if not args.check:
        OUTPUT.write_text(generated, encoding="utf-8")
        print(f"[notices] wrote {OUTPUT.relative_to(ROOT)} ({len(generated)} bytes)")
        return 0

    if not OUTPUT.exists():
        print(f"FAIL: {OUTPUT.relative_to(ROOT)} does not exist", file=sys.stderr)
        print("      run `just notices`", file=sys.stderr)
        return 1

    # Bytes, not read_text: the guard's whole job is a byte-exact
    # comparison, and text mode would translate line endings on the way
    # in and hide exactly the drift it is looking for.
    current = OUTPUT.read_bytes().decode("utf-8")
    if current == generated:
        print(f"[notices] {OUTPUT.relative_to(ROOT)} is current")
        return 0

    diff = difflib.unified_diff(
        current.splitlines(keepends=True),
        generated.splitlines(keepends=True),
        fromfile=f"a/{OUTPUT.name} (checked in)",
        tofile=f"b/{OUTPUT.name} (regenerated from Cargo.lock)",
        n=2,
    )
    # Bounded: a lockfile update can move hundreds of crates, and the
    # useful part of that diff is its first screen.
    shown = 0
    for line in diff:
        sys.stderr.write(line)
        shown += 1
        if shown >= 200:
            sys.stderr.write("... (diff truncated)\n")
            break
    print(
        f"\nFAIL: {OUTPUT.relative_to(ROOT)} does not match the dependency graph.\n"
        "      A dependency changed without the notices being regenerated, so\n"
        "      the published binaries would ship a licence list that no longer\n"
        "      describes what is inside them (Codeberg #288).\n"
        "      Fix: `just notices` and commit the result.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
