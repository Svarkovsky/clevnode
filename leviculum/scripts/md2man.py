#!/usr/bin/env python3
"""Render the manual pages in docs/src/man/ to roff.

The pages are written once, as Markdown, so that mdbook can publish them
and the Debian packages can install them. This script is the second half
of that: it converts the narrow Markdown subset those pages use into
man(7) roff. It is deliberately not a general Markdown implementation —
pandoc would be, but it is a large build dependency for six files, and
the pages only ever use six constructs:

    # name(1)                 the title line, one per file
    ## SECTION / ### Sub      section and subsection headings
    term                      a definition list, pandoc style:
    :   definition            the term line followed by ":   "
    **bold** *italic* `code`  inline emphasis
    <four-space indent>       a literal block
    blank-line-separated      paragraphs

Anything else passes through as text. If a page ever needs more, extend
this rather than reaching for a converter: the point is that the Debian
packages build with nothing but python3, which the test harness already
requires.

Usage:
    scripts/md2man.py --outdir DIR SRC.1.md [SRC.1.md ...]

Each source is written to DIR as its own basename minus the .md, so
docs/src/man/lnsd.1.md becomes DIR/lnsd.1.

The .TH date comes from SOURCE_DATE_EPOCH when set, so a reproducible
build produces byte-identical pages.
"""

import argparse
import datetime
import os
import re
import sys
from pathlib import Path

MANUAL = "Leviculum Manual"
SOURCE = "Leviculum"

# Applied before the hyphen escape below, so the dashes here are not
# themselves rewritten into \-.
TYPOGRAPHY = (
    ("—", "\\(em"),
    ("–", "\\(en"),
    ("‘", "\\(oq"),
    ("’", "\\(cq"),
    ("“", "\\(lq"),
    ("”", "\\(rq"),
    ("…", "\\&..."),
    (" ", "\\ "),
)


def build_date() -> str:
    """Date for the .TH line, honouring SOURCE_DATE_EPOCH."""
    epoch = os.environ.get("SOURCE_DATE_EPOCH")
    when = (
        datetime.datetime.fromtimestamp(int(epoch), datetime.timezone.utc)
        if epoch
        else datetime.datetime.now(datetime.timezone.utc)
    )
    return when.strftime("%Y-%m-%d")


def escape(text: str) -> str:
    r"""Escape roff specials in body text.

    Backslash first, or the escapes we add below would be re-escaped.
    A hyphen must become \- to stay a hyphen rather than turning into a
    typographic minus that breaks copy-pasted option names.
    """
    text = text.replace("\\", "\\e")
    # Typographic characters the pages pick up from prose. groff reads the
    # file as ASCII, so anything above 0x7f has to become a named escape
    # or it warns and drops the character.
    for char, esc in TYPOGRAPHY:
        text = text.replace(char, esc)
    text = text.replace("-", "\\-")
    # "name -- description" is how the NAME section reads in Markdown; man
    # pages spell that separator as a single \-. Only the spaced form is
    # rewritten, so option names like --config keep both hyphens.
    text = text.replace(" \\-\\- ", " \\- ")
    return text


# Stands in for a literal backtick between the double- and single-backtick
# passes in `inline`. A control character no man page source contains, so it
# cannot collide with real text.
LITERAL_BACKTICK = "\x00"


def inline(text: str) -> str:
    """Convert **bold**, *italic* and `code` to roff escapes.

    Code renders bold, which is what man(7) pages conventionally do for
    literal option and file names.

    Double-backtick spans are handled first, so a code span may contain a
    literal backtick — as Markdown itself allows, and as a NomadNet URL's
    field separator needs. Its content is stashed behind
    `LITERAL_BACKTICK` before the single-backtick pass runs, because a raw
    backtick left in place is picked up there as an opening fence: it pairs
    with the next one further along the line and every code span after it
    shifts by one. That is how the tail of lnomad(1)'s url description came
    to render with its bold inverted and a stray backtick in the prose.
    """
    text = escape(text)
    text = re.sub(r"\*\*(.+?)\*\*", r"\\fB\1\\fR", text)
    text = re.sub(
        r"``(.+?)``",
        lambda m: "\\fB" + m.group(1).replace("`", LITERAL_BACKTICK) + "\\fR",
        text,
    )
    text = re.sub(r"`(.+?)`", r"\\fB\1\\fR", text)
    text = re.sub(r"(?<!\*)\*([^*]+?)\*(?!\*)", r"\\fI\1\\fR", text)
    return text.replace(LITERAL_BACKTICK, "`")


def protect(line: str) -> str:
    """Keep a line starting with . or ' from being read as a macro."""
    return "\\&" + line if line[:1] in (".", "'") else line


class Renderer:
    def __init__(self) -> None:
        self.out: list[str] = []
        # A definition list emits .TP before its term; paragraphs inside
        # the definition must not restart it, so we track whether the
        # previous emitted block was a definition body.
        self.pending_term: str | None = None
        # SYNOPSIS is the one section where a line break carries meaning:
        # each line is a separate way to invoke the program. Markdown would
        # reflow them into one paragraph, so they get an explicit .br.
        self.section: str = ""

    def emit(self, line: str) -> None:
        self.out.append(line)

    def flush_term(self) -> None:
        if self.pending_term is not None:
            self.emit(".TP")
            self.emit(protect(inline(self.pending_term)))
            self.pending_term = None

    def render(self, text: str, name: str, section: str) -> str:
        self.emit(f'.TH {name.upper()} {section} "{build_date()}" "{SOURCE}" "{MANUAL}"')
        lines = text.splitlines()
        i = 0
        para: list[str] = []

        def flush_para() -> None:
            if not para:
                return
            self.flush_term()
            if not self.out[-1].startswith((".TP", ".SH", ".SS")):
                self.emit(".PP")
            if self.section == "SYNOPSIS":
                for n, one in enumerate(para):
                    if n:
                        self.emit(".br")
                    self.emit(protect(inline(one)))
            else:
                self.emit(protect(inline(" ".join(para))))
            para.clear()

        while i < len(lines):
            line = lines[i]
            stripped = line.strip()

            # Title: consumed by the caller for .TH, skipped here.
            if line.startswith("# "):
                i += 1
                continue

            if line.startswith("### "):
                flush_para()
                self.flush_term()
                self.emit(".SS " + inline(line[4:].strip()))
                i += 1
                continue

            if line.startswith("## "):
                flush_para()
                self.flush_term()
                self.section = line[3:].strip().upper()
                self.emit(".SH " + escape(self.section))
                i += 1
                continue

            # Literal block: four-space indent, not a definition body.
            if line.startswith("    ") and stripped and not stripped.startswith(":"):
                flush_para()
                self.flush_term()
                self.emit(".RS 4")
                self.emit(".nf")
                while i < len(lines) and (
                    lines[i].startswith("    ") or not lines[i].strip()
                ):
                    if lines[i].strip():
                        self.emit(protect(escape(lines[i][4:])))
                    elif i + 1 < len(lines) and lines[i + 1].startswith("    "):
                        self.emit("")
                    else:
                        break
                    i += 1
                self.emit(".fi")
                self.emit(".RE")
                continue

            # Definition body: ":   text", attached to the term above.
            if stripped.startswith(":"):
                body = stripped[1:].strip()
                self.flush_term()
                self.emit(protect(inline(body)))
                i += 1
                continue

            if not stripped:
                flush_para()
                i += 1
                continue

            # A term is a plain line whose successor is a definition body.
            if i + 1 < len(lines) and lines[i + 1].strip().startswith(":"):
                flush_para()
                self.flush_term()
                self.pending_term = stripped
                i += 1
                continue

            para.append(stripped)
            i += 1

        flush_para()
        self.flush_term()
        return "\n".join(self.out) + "\n"


TITLE_RE = re.compile(r"^#\s+([A-Za-z0-9_.-]+)\((\d)\)\s*$")


def convert(src: Path) -> str:
    text = src.read_text(encoding="utf-8")
    first = text.splitlines()[0] if text.splitlines() else ""
    match = TITLE_RE.match(first)
    if not match:
        raise SystemExit(f"{src}: first line must be '# name(section)', got {first!r}")
    return Renderer().render(text, match.group(1), match.group(2))


def selftest() -> int:
    """Check the inline conversions that are easy to get subtly wrong.

    A mangled man page does not fail a build, it just ships: nothing
    downstream reads roff. These cases are the cheap standing guard, run
    from the Justfile before every page is rendered.
    """
    cases = [
        # A code span may hold a literal backtick, and must not leave one
        # behind to be read as an opening fence by the single-backtick pass.
        # The trailing spans decide it: they are what shifted before.
        (
            "as ``a[`f=v]`` where `x` and `y`",
            "as \\fBa[`f=v]\\fR where \\fBx\\fR and \\fBy\\fR",
        ),
        ("plain `code` here", "plain \\fBcode\\fR here"),
        ("**bold** and *italic*", "\\fBbold\\fR and \\fIitalic\\fR"),
    ]
    failed = 0
    for src, want in cases:
        got = inline(src)
        if got != want:
            print(f"[md2man] selftest FAILED for {src!r}\n  want: {want!r}\n  got:  {got!r}")
            failed += 1
    if failed:
        return 1
    print(f"[md2man] selftest ok ({len(cases)} cases)")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--outdir", type=Path, help="directory to write the roff pages into"
    )
    parser.add_argument(
        "--selftest",
        action="store_true",
        help="check the inline conversions and exit, rendering nothing",
    )
    parser.add_argument("sources", nargs="*", type=Path)
    args = parser.parse_args()

    if args.selftest:
        return selftest()
    if args.outdir is None or not args.sources:
        parser.error("--outdir and at least one source are required")

    args.outdir.mkdir(parents=True, exist_ok=True)
    for src in args.sources:
        out = args.outdir / src.name.removesuffix(".md")
        out.write_text(convert(src), encoding="utf-8")
        print(f"[md2man] {src} -> {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
