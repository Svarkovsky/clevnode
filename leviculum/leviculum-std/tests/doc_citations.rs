//! Citation guard (Guarantee C): a reference to something outside the text
//! must still point at what it claims. Covers `docs/src/**/*.md` and the
//! Rust sources and tests of `leviculum-core`, `leviculum-lxmf`,
//! `leviculum-lxmf-node` and `leviculum-std`; and every concept document
//! must be reachable from `docs/src/SUMMARY.md`.
//!
//! The concept documents are binding policy, and a wrong citation gets
//! believed. A 2026-07 audit found six drifted citations across five
//! documents after roughly one month; nothing else catches this, because
//! a drifted citation looks exactly like a fresh one. Source citations are
//! the load-bearing half: a pinned deviation means nothing without the
//! reference line it deviates from.
//!
//! # Three kinds of sentence, and which of them is checked
//!
//! A reader of a doc comment cannot tell by looking which of these they are
//! in front of, so this is the map.
//!
//! **1. A `path:line` citation — resolved.** Per citation:
//! - the cited file exists in the repo and has at least the cited number
//!   of lines (catches deletions and renames);
//! - where the citation *names what it points at* — a backticked
//!   identifier attached to it, see below — that identifier occurs within
//!   `WINDOW` lines of the cited span (catches drift).
//!
//! A bare citation that names nothing only gets the existence check. Both
//! kinds are counted and printed so the coverage is visible: run with
//! `--nocapture` to see the counts.
//!
//! ## What "names what it points at" means
//!
//! A line number in a moving tree is not a durable anchor: every batch that
//! inserts a line above the cited one silently repoints the citation at
//! whatever now sits there. Existence-checking a line number cannot see
//! that, because the line still exists — it is simply about something else.
//! The name is what survives the move, so a citation that carries one can
//! be checked and a citation that carries none cannot.
//!
//! Two spellings count as attaching a name, and both are what the corpus
//! already writes:
//!
//! ```text
//! `resolve_lt_alock` (`leviculum-std/src/driver/mod.rs:353`)   -- paren
//! (`resolve_lt_alock`, `leviculum-std/src/driver/mod.rs:353`)  -- comma
//! ```
//!
//! Nothing but whitespace may sit between the name and the citation, so
//! the pairing is unambiguous: an identifier mentioned earlier in the
//! sentence is not read as the citation's subject. A token that is itself
//! a citation (`Destination.py:322`) or that carries no letter in its last
//! segment (a `1209:0001` USB VID:PID) is not an identifier and does not
//! attach — those sit next to citations in tables and would otherwise be
//! read as the subject of the citation beside them.
//!
//! The comma spelling was admitted in 2026-08 after `lora.rs:725` drifted
//! onto radio-init code inside a *regulatory* claim and this guard passed
//! it. Adding it converted 75 book and 55 source citations from
//! existence-checked to drift-checked without editing one of them, and
//! immediately reported 30 that had drifted. Which is the argument for
//! spelling the shape the tree already uses rather than inventing a new
//! one: a scheme that needs every citation rewritten by hand is a
//! migration, and gets done never.
//!
//! A citation into a `reference/` submodule that is not checked out is a
//! *different* failure from a drifted one, and says so: nothing is wrong
//! with the citation, the reference is simply absent. Whether a checked-out
//! reference is at the commit this tree pins is not checkable from here —
//! that is `scripts/check-submodule-pins.sh`, in a gate rather than in a
//! test. See `docs/src/concepts/checks-and-citations.md`.
//!
//! **2. A prose attribution to a document — checked for figures only.** A
//! Rust doc-comment paragraph that names a document under `docs/` and
//! quotes a decimal figure must have that figure occur in that document
//! (Codeberg #200). It carries no line and no identifier, so nothing above
//! reaches it: `PROCESSOR_TICK_BUDGET` was justified with "the number comes
//! off `docs/…/core-lock-budget.md`" and then named 126.6 ms, a figure that
//! existed nowhere in the tree but in that comment. The measurement was
//! real; the attribution was not.
//!
//! Only decimal figures, and only within one paragraph. See
//! `figure_attributions` for exactly what that excludes and why the trigger
//! is drawn where it is.
//!
//! **3. Everything else in a doc comment — unchecked prose.** Which is most
//! of it. A sentence can name a mechanism that no longer exists, describe a
//! guarantee the code does not make, or attribute an integer to a page that
//! never carried it, and nothing here will notice. Guarantee C is about
//! references, not about truth.

use regex::Regex;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Identifier search window in lines around the cited span.
///
/// Measured on the corpus at introduction time (2026-08): every correct
/// citation had its identifier within 5 lines of the cited line (doc
/// comments, attributes and derives legitimately sit between a cited
/// first line and the item name), while every genuinely drifted citation
/// was off by 11 lines or more. 8 splits the measured gap with margin on
/// both sides.
const WINDOW: usize = 8;

/// Path prefixes that refer to sibling repositories. These citations are
/// counted and reported but cannot be existence-checked from this
/// workspace.
const EXTERNAL_PREFIXES: &[&str] = &["periculum/", "ble-reticulum/"];

/// The vendored references. A citation into one of these that is not
/// checked out fails differently from a citation that has drifted.
const SUBMODULES: &[&str] = &["Reticulum", "LXMF", "LXST", "RNode_Firmware"];

/// Crate roots whose Rust sources carry citations. Whole crate directories,
/// not `src/` alone: `tests/`, `examples/` and `benches/` cite the reference
/// as much as `src/` does, and narrowing the glob would only make the number
/// smaller, not the tree more correct.
const SOURCE_CRATES: &[&str] = &[
    "leviculum-core",
    "leviculum-lxmf",
    "leviculum-lxmf-node",
    "leviculum-std",
];

/// Extensions that make a `name.ext:number` look like a citation while being
/// a `host:port`. Only reachable in `Corpus::Source`, where citations are not
/// backticked and a config example like `peer.example.com:5000` sits in an
/// ordinary string literal. A blocklist rather than an extension allowlist on
/// purpose: an allowlist silently drops a citation into a file type nobody
/// thought of, this can only produce visible noise.
const NON_FILE_EXTENSIONS: &[&str] = &[
    "com", "net", "org", "io", "dev", "local", "onion", "i2p", "de",
];

/// Directories never walked, for either the corpus or the resolution index.
/// `docs/book` would shadow the doc sources; `target` holds build output and
/// vendored source copies.
const SKIP_DIRS: &[&str] = &[".git", "target", "book", "node_modules"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate dir has a parent")
        .to_path_buf()
}

fn walk(dir: &Path, skip: &[&str], out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if skip.contains(&name.as_ref()) {
                continue;
            }
            walk(&path, skip, out);
        } else {
            out.push(path);
        }
    }
}

/// Which corpus a citation was found in, and therefore how it is spelled.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Corpus {
    /// Markdown prose: a citation is always inside backticks.
    Book,
    /// Rust source: citations live in comments *and* in assertion messages
    /// (`"... (Transport.py:2176)"`), and are usually not backticked. The
    /// pattern therefore matches bare, which is why the path shape has to
    /// carry the whole burden of not matching ordinary code.
    Source,
}

/// One `path:line[-line][,line[-line]…]` citation found in a file.
struct Citation {
    doc: PathBuf,
    doc_line: usize,
    raw: String,
    path: String,
    /// Inclusive line spans: `329` → [(329,329)], `155-199,204` →
    /// [(155,199),(204,204)].
    spans: Vec<(usize, usize)>,
    /// The backticked identifier attached to the citation, if it names
    /// what it points at. See [`attached_ident`].
    ident: Option<String>,
}

/// Directories whose files carry no extension, admitted by their directory.
///
/// The extension requirement below is load-bearing, so extensionless files
/// cannot simply be let in: dropping it would match `127.0.0.1:4242`, Python
/// slices, and any prose word followed by a number, and a guard with false
/// positives gets switched off (`docs/src/concepts/checks-and-citations.md`).
/// The hook scripts have neither an extension nor a name the pattern knows,
/// so until Codeberg #213 the guard's coverage of them was exactly zero: the
/// best-written possible citation into a hook was silently skipped while the
/// identical citation one file over resolved.
///
/// Admitting them by directory keeps the pattern as tight as it was —
/// `.githooks/` matches nothing else in the corpus, where a bare
/// extensionless token would match ordinary prose. It fails closed like
/// `NON_FILE_EXTENSIONS` beside it: a new extensionless file outside these
/// directories is uncovered rather than falsely flagged.
const EXTENSIONLESS_DIRS: &[&str] = &[".githooks"];

/// A path with a letters-only extension (or the extensionless `Justfile`, or
/// a file under an [`EXTENSIONLESS_DIRS`] directory) followed by `:` and a
/// line spec. The extension must be alphabetic so `127.0.0.1:4242` and Python
/// slices like `packed[:16]` do not match.
///
/// `word` is the left-hand boundary, and it sits *inside* the alternation
/// rather than in front of it. `Corpus::Source` matches bare, so a longer
/// word ending in a cited filename must not match at the inner offset — but
/// a `.githooks/` citation begins with a `.`, and `\b` before that only
/// matches when the preceding character is a word character, which after a
/// space or at the start of a comment it is not.
fn path_pattern(corpus: Corpus) -> String {
    let word = match corpus {
        Corpus::Source => r"\b",
        Corpus::Book => "",
    };
    let mut alts = vec![
        format!(r"{word}[A-Za-z0-9_][A-Za-z0-9_./-]*\.[A-Za-z]+"),
        format!("{word}Justfile"),
    ];
    alts.extend(
        EXTENSIONLESS_DIRS
            .iter()
            .map(|dir| format!(r"{}/[A-Za-z0-9_-]+", regex::escape(dir))),
    );
    format!("({})", alts.join("|"))
}

const SPEC_PATTERN: &str = r"(\d+(?:-\d+)?(?:,\d+(?:-\d+)?)*)";

fn cite_regex(corpus: Corpus) -> Regex {
    let body = format!("{}:{SPEC_PATTERN}", path_pattern(corpus));
    match corpus {
        Corpus::Book => Regex::new(&format!("`{body}`")).unwrap(),
        // The right side is anchored by the line spec; the left by the `\b`
        // inside `path_pattern`.
        Corpus::Source => Regex::new(&body).unwrap(),
    }
}

/// The two spellings that attach a name to the citation that follows:
/// ``ident` (` and `` `ident`, ``. Whitespace (including line breaks) may
/// sit between; nothing else may. A trailing `()` (function spelling) is
/// stripped.
fn ident_regexes() -> [Regex; 2] {
    [
        Regex::new(r"`([A-Za-z0-9_:.]+)(?:\(\))?`\s*\(\s*$").unwrap(),
        Regex::new(r"`([A-Za-z0-9_:.]+)(?:\(\))?`\s*,\s*$").unwrap(),
    ]
}

/// A backticked token that is itself a citation: `Destination.py:322`,
/// `Justfile:719`. Tables list these next to each other, so without this
/// the second citation of a row would take the first as its subject.
fn citation_shaped() -> Regex {
    Regex::new(r"(?:\.[A-Za-z]+|^Justfile):\d").unwrap()
}

/// Whether `token` can be the name of a code item.
///
/// Fails closed, on both counts a real corpus supplies: a token that is
/// itself a citation, and one whose last segment carries no letter — a
/// `1209:0001` USB VID:PID, a `4.2:1` ratio. Neither can be searched for
/// as an identifier, and reading either as one would count a citation as
/// drift-checked while checking nothing.
fn looks_like_identifier(token: &str, citation_shaped: &Regex) -> bool {
    !citation_shaped.is_match(token)
        && token
            .rsplit([':', '.'])
            .next()
            .is_some_and(|seg| seg.chars().any(|c| c.is_ascii_alphabetic()))
}

/// The identifier the citation starting at the end of `before` names, if
/// any.
fn attached_ident(before: &str, idents: &[Regex; 2], citation_shaped: &Regex) -> Option<String> {
    idents
        .iter()
        .find_map(|re| re.captures(before))
        .map(|c| c[1].to_string())
        .filter(|token| looks_like_identifier(token, citation_shaped))
}

/// Scheme-relative or absolute URLs contain `host.tld` shapes that the path
/// pattern would otherwise accept. Only reachable in `Corpus::Source`, where
/// citations are not backticked.
fn inside_url(text: &str, start: usize) -> bool {
    let before = &text[..start];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let line = &before[line_start..];
    match line.rfind("//") {
        // A URL has no whitespace between `//` and the match.
        Some(i) => !line[i..].contains(char::is_whitespace) && line[..i].ends_with(':'),
        None => false,
    }
}

fn scan(root: &Path, files: &[PathBuf], corpus: Corpus) -> Vec<Citation> {
    let cite_re = cite_regex(corpus);
    let ident_res = ident_regexes();
    let citation_shaped_re = citation_shaped();
    let mut citations = Vec::new();
    for file in files {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        for m in cite_re.captures_iter(&text) {
            let whole = m.get(0).unwrap();
            if corpus == Corpus::Source {
                if inside_url(&text, whole.start()) {
                    continue;
                }
                let ext = m[1].rsplit('.').next().unwrap_or_default();
                if NON_FILE_EXTENSIONS.contains(&ext) {
                    continue;
                }
            }
            let spans = m[2]
                .split(',')
                .map(|part| match part.split_once('-') {
                    Some((a, b)) => (a.parse().unwrap(), b.parse().unwrap()),
                    None => {
                        let n = part.parse().unwrap();
                        (n, n)
                    }
                })
                .collect();
            citations.push(Citation {
                doc: file.strip_prefix(root).unwrap_or(file).to_path_buf(),
                doc_line: text[..whole.start()].matches('\n').count() + 1,
                raw: whole.as_str().to_string(),
                path: m[1].to_string(),
                spans,
                ident: attached_ident(&text[..whole.start()], &ident_res, &citation_shaped_re),
            });
        }
    }
    citations
}

/// Every `docs/src/**/*.md` citation.
fn book_citations(root: &Path) -> Vec<Citation> {
    let mut mds = Vec::new();
    walk(&root.join("docs/src"), SKIP_DIRS, &mut mds);
    mds.retain(|p| p.extension().is_some_and(|e| e == "md"));
    mds.sort();
    assert!(
        !mds.is_empty(),
        "no markdown files under docs/src -- wrong repo root?"
    );
    scan(root, &mds, Corpus::Book)
}

/// Every `*.rs` citation in the three cited crates.
fn source_citations(root: &Path, crates: &[&str]) -> Vec<Citation> {
    let mut rs = Vec::new();
    for krate in crates {
        walk(&root.join(krate), SKIP_DIRS, &mut rs);
    }
    rs.retain(|p| p.extension().is_some_and(|e| e == "rs"));
    // The canary fixtures carry deliberately drifted citations; they are
    // the guard's own input, not part of the corpus it guards.
    rs.retain(|p| !p.components().any(|c| c.as_os_str() == "citation_canary"));
    rs.sort();
    assert!(
        !rs.is_empty(),
        "no Rust sources under {crates:?} -- wrong repo root?"
    );
    scan(root, &rs, Corpus::Source)
}

/// Distance in lines from `line` to the nearest edge of `span` (0 when
/// inside).
fn span_distance(line: usize, span: (usize, usize)) -> usize {
    if line < span.0 {
        span.0 - line
    } else {
        line.saturating_sub(span.1)
    }
}

/// Whether the cited span lies inside the block the identifier at `hit`
/// introduces. Both are 1-based line numbers.
///
/// The second thing a citation means. `` `remember_ticket`,
/// `LXMRouter.py:1102-1105` `` points at the item itself and the
/// adjacency window sees it; `` `Transport.request_path`,
/// `Transport.py:2786-2787` `` names the *enclosing* function and points at
/// two statements 15 lines into its body, which adjacency cannot see and
/// which is just as much "the citation names what it points at". Both
/// spellings are in the corpus in roughly equal numbers, so a rule that
/// only understood the first would report every instance of the second —
/// and a guard with false positives gets switched off.
///
/// Indentation rather than syntax, so this needs no parser and holds for
/// Rust, Python, C and the Arduino sources alike: the line that names an
/// item is less indented than every line of its body. Blank lines carry no
/// indentation of their own and are skipped.
///
/// What it gives up: an identifier that merely occurs on some line above
/// the span, with the span nested deeper, satisfies this without being the
/// enclosing item — a `let` binding above a block, say. So this is a
/// weaker check than adjacency, not a stronger one. It is still the
/// difference between "the citation is somewhere in the named item" and
/// "the citation is somewhere in the file", which is what the existence
/// check already was.
fn encloses(lines: &[&str], hit: usize, span_start: usize) -> bool {
    if hit >= span_start || hit == 0 || span_start > lines.len() {
        return false;
    }
    let indent = |l: &&str| l.len() - l.trim_start().len();
    let base = indent(&lines[hit - 1]);
    lines[hit..span_start]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .all(|l| indent(l) > base)
}

#[derive(Debug, PartialEq, Eq)]
enum FailureKind {
    /// Nothing in the tree matches the cited path.
    Missing,
    /// The path names a `reference/` submodule that is not checked out.
    /// Not the citation's fault, and not fixable by editing it.
    SubmoduleAbsent,
    /// The file is there and long enough, but the cited identifier is not
    /// near the cited line.
    Drift,
}

struct Failure {
    kind: FailureKind,
    message: String,
}

/// A resolved identifier citation and how many lines its identifier sits
/// from the cited span. Zero is exact; anything else is `WINDOW` budget
/// already spent at landing time, and a citation that lands at the edge
/// reddens on the next unrelated insertion above it (which is how 33
/// citations born at +7/+8 in one commit all tipped over when a later
/// commit added 4 lines).
struct Offset {
    where_: String,
    dist: usize,
    nearest: usize,
}

#[derive(Default)]
struct Counts {
    with_ident: usize,
    bare: usize,
    external: usize,
    /// One entry per identifier citation that resolved by adjacency.
    /// Citations that resolve only by enclosure carry no meaningful
    /// offset (they point at statements inside the named item) and are
    /// not listed.
    offsets: Vec<Offset>,
}

impl Counts {
    fn total(&self) -> usize {
        self.with_ident + self.bare + self.external
    }
}

/// The offset histogram for a corpus, plus every non-exact citation by
/// name. Run with `--nocapture` to see it. A green guard hides how much
/// drift budget is already spent; this is what makes it visible.
fn report_offsets(label: &str, counts: &Counts) {
    let mut hist: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    for o in &counts.offsets {
        *hist.entry(o.dist).or_default() += 1;
    }
    let buckets: Vec<String> = hist.iter().map(|(d, n)| format!("+{d}:{n}")).collect();
    println!(
        "{label} identifier-citation offsets (lines from cited span to nearest \
         identifier, +0 = exact, tolerance {WINDOW}): {}",
        buckets.join(" ")
    );
    for o in counts.offsets.iter().filter(|o| o.dist > 0) {
        println!(
            "  +{}: {} (identifier at line {})",
            o.dist, o.where_, o.nearest
        );
    }
}

/// The submodules under `<root>/reference` that have no working tree.
fn absent_submodules(root: &Path) -> BTreeSet<&'static str> {
    SUBMODULES
        .iter()
        .copied()
        .filter(|s| {
            fs::read_dir(root.join("reference").join(s))
                .map(|mut d| d.next().is_none())
                .unwrap_or(true)
        })
        .collect()
}

/// The single checking core, shared by the book guard, the source guard and
/// the canary. `root` is both the resolution root and the prefix stripped
/// from reported paths.
fn check(root: &Path, citations: &[Citation]) -> (Counts, Vec<Failure>) {
    let mut files = Vec::new();
    walk(root, SKIP_DIRS, &mut files);
    let rel_files: Vec<String> = files
        .iter()
        .map(|p| {
            p.strip_prefix(root)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    // Bare filenames like `transport.rs:207` resolve by suffix.
    let resolve = |cited: &str| -> Vec<&str> {
        let suffix = format!("/{cited}");
        rel_files
            .iter()
            .filter(|f| *f == cited || f.ends_with(&suffix))
            .map(String::as_str)
            .collect()
    };

    let absent = absent_submodules(root);
    // A citation spelled `reference/LXMF/...` names its submodule outright.
    // A bare `LXMRouter.py:123` does not, so an absent submodule can only be
    // offered as a caveat on the missing-file failure.
    let named_submodule = |path: &str| -> Option<&'static str> {
        let rest = path.strip_prefix("reference/")?;
        SUBMODULES
            .iter()
            .copied()
            .find(|s| rest.strip_prefix(s).is_some_and(|r| r.starts_with('/')))
    };

    let mut counts = Counts::default();
    let mut failures = Vec::new();

    for c in citations {
        if EXTERNAL_PREFIXES.iter().any(|p| c.path.starts_with(p)) {
            counts.external += 1;
            continue;
        }
        match c.ident {
            Some(_) => counts.with_ident += 1,
            None => counts.bare += 1,
        }
        let where_ = format!("{}:{}: {}", c.doc.display(), c.doc_line, c.raw);

        if let Some(sub) = named_submodule(&c.path) {
            if absent.contains(sub) {
                failures.push(Failure {
                    kind: FailureKind::SubmoduleAbsent,
                    message: format!(
                        "{where_}\n    reference/{sub} is not checked out, so this citation \
                         cannot be verified.\n    This is NOT a drifted citation -- do not edit \
                         it. Check the reference out:\n        git submodule update --init \
                         reference/{sub}\n    (that the checkout matches the gitlink is a \
                         separate check: scripts/check-submodule-pins.sh)"
                    ),
                });
                continue;
            }
        }

        let candidates = resolve(&c.path);
        if candidates.is_empty() {
            let hint = if absent.is_empty() {
                String::new()
            } else {
                format!(
                    "\n    (reference/{} not checked out -- if this cites one of them, it is \
                     absent rather than drifted; `git submodule update --init` before trusting \
                     this failure)",
                    absent
                        .iter()
                        .copied()
                        .collect::<Vec<_>>()
                        .join(", reference/")
                )
            };
            failures.push(Failure {
                kind: FailureKind::Missing,
                message: format!(
                    "{where_}\n    no file matching `{}` in the repo -- deleted or renamed?{hint}",
                    c.path
                ),
            });
            continue;
        }

        // A citation passes if any candidate file satisfies every check;
        // bare filenames can be ambiguous (two `constants.rs` exist) and
        // the prose, not the path, disambiguates.
        let max_line = c.spans.iter().map(|s| s.1).max().unwrap();
        let mut candidate_notes = Vec::new();
        let mut passed = false;
        for cand in &candidates {
            let text = fs::read_to_string(root.join(cand)).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            if lines.len() < max_line {
                candidate_notes.push(format!(
                    "    {cand} has only {} lines (cited: {max_line})",
                    lines.len()
                ));
                continue;
            }
            let Some(ident) = &c.ident else {
                passed = true;
                break;
            };
            // `Type::method` / `module.attr` cite the item; the source
            // line contains the last segment.
            let needle = ident.rsplit(&[':', '.'][..]).next().unwrap();
            let hits: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.contains(needle))
                .map(|(i, _)| i + 1)
                .collect();
            let resolved = hits.iter().any(|&h| {
                c.spans
                    .iter()
                    .any(|&span| span_distance(h, span) <= WINDOW || encloses(&lines, h, span.0))
            });
            if resolved {
                // `resolved` implies at least one hit, and every citation
                // carries at least one span.
                let (nearest, dist) = hits
                    .iter()
                    .map(|&h| {
                        (
                            h,
                            c.spans.iter().map(|&s| span_distance(h, s)).min().unwrap(),
                        )
                    })
                    .min_by_key(|&(_, d)| d)
                    .unwrap();
                if dist <= WINDOW {
                    counts.offsets.push(Offset {
                        where_: where_.clone(),
                        dist,
                        nearest,
                    });
                }
                passed = true;
                break;
            }
            let cited_first = c.spans[0].0;
            let nearest = hits
                .iter()
                .min_by_key(|&&h| c.spans.iter().map(|&s| span_distance(h, s)).min().unwrap());
            candidate_notes.push(match nearest {
                Some(&n) => format!(
                    "    `{needle}` not within {WINDOW} lines of the cited span in {cand}\n    cited line {cited_first}: {}\n    nearest `{needle}`: line {n}: {}",
                    lines[cited_first - 1].trim(),
                    lines[n - 1].trim()
                ),
                None => format!("    `{needle}` does not occur anywhere in {cand}"),
            });
        }
        if !passed {
            failures.push(Failure {
                kind: FailureKind::Drift,
                message: format!("{where_}\n{}", candidate_notes.join("\n")),
            });
        }
    }

    (counts, failures)
}

// --- figure attribution (Codeberg #200) ----------------------------------

/// A document path under `docs/` ending in `.md`. Any `:line` suffix stops
/// the match on its own, so the same reference is seen whether or not it
/// carries one.
const DOC_PATH_PATTERN: &str = r"[A-Za-z0-9_.-]*docs/[A-Za-z0-9_./-]*\.md";

/// A decimal figure. Boundaries are applied separately in `figures_in`,
/// because the `regex` crate has no lookaround.
const FIGURE_PATTERN: &str = r"\d+\.\d+";

/// One `///`/`//!` paragraph: the contiguous doc-comment lines between two
/// blank doc-comment lines (or between a blank one and the end of the run).
struct Paragraph {
    file: PathBuf,
    /// Line number of the paragraph's first line, in the source file.
    first_line: usize,
    text: String,
}

/// Every doc-comment paragraph in `files`.
///
/// Paragraph, not sentence, and that is the load-bearing choice. The #200
/// defect attributed its figure across a sentence boundary — "The number
/// comes off `docs/…`" in one sentence, "The failure mode it names is
/// 126.6 ms" two sentences later — so a sentence-scoped trigger would have
/// sailed past the case it exists for. Paragraph scope also avoids having
/// to segment sentences at all, which in this corpus means deciding whether
/// the `.` in `126.6`, in `core-lock-budget.md` and in `e.g.` ends one.
fn doc_paragraphs(root: &Path, files: &[PathBuf]) -> Vec<Paragraph> {
    let mut out = Vec::new();
    for file in files {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        let rel = file.strip_prefix(root).unwrap_or(file).to_path_buf();
        let mut acc: Vec<&str> = Vec::new();
        let mut first = 0usize;
        let flush = |acc: &mut Vec<&str>, first: usize, out: &mut Vec<Paragraph>| {
            if !acc.is_empty() {
                out.push(Paragraph {
                    file: rel.clone(),
                    first_line: first,
                    text: acc.join(" "),
                });
                acc.clear();
            }
        };
        for (i, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            let body = trimmed
                .strip_prefix("///")
                .or_else(|| trimmed.strip_prefix("//!"))
                .map(str::trim);
            match body {
                Some("") | None => flush(&mut acc, first, &mut out),
                Some(b) => {
                    if acc.is_empty() {
                        first = i + 1;
                    }
                    acc.push(b);
                }
            }
        }
        flush(&mut acc, first, &mut out);
    }
    out
}

/// Whether the byte range `[start, end)` of `text` is a standalone number:
/// not glued to another digit or to a further `.`.
///
/// This is what keeps `0.8.0`, `1.3.4` and `127.0.0.1` out. Each yields
/// `0.8` / `1.3` / `127.0` from the pattern and each is rejected here for
/// the `.` that follows.
fn is_standalone(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    let glued = |c: Option<char>| c.is_some_and(|c| c.is_ascii_digit() || c == '.');
    !glued(before) && !glued(after)
}

/// The standalone decimal figures in `text`, in order, deduplicated.
fn figures_in(re: &Regex, text: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    re.find_iter(text)
        .filter(|m| is_standalone(text, m.start(), m.end()))
        .map(|m| m.as_str().to_string())
        .filter(|f| seen.insert(f.clone()))
        .collect()
}

/// Whether `figure` occurs in `text` as a standalone number.
fn document_names_figure(re: &Regex, text: &str, figure: &str) -> bool {
    re.find_iter(text)
        .any(|m| m.as_str() == figure && is_standalone(text, m.start(), m.end()))
}

/// Every decimal figure a doc comment attributes to a named document must
/// occur in that document.
///
/// Returns `(paragraphs triggered, figures checked, failures)`.
///
/// # Where the trigger is drawn, and what that gives up
///
/// A guard with false positives gets switched off, and a switched-off guard
/// is worse than none, so this is narrow on purpose and the cost is stated
/// rather than hidden.
///
/// It fires only where **a paragraph names a document under `docs/` and
/// quotes a decimal figure**. Decimal, because that is what separates a
/// measurement somebody took from a number somebody derived in the same
/// breath. In the paragraph the #200 defect lived in, "126.6 ms", "3.2 ms"
/// and "0.8 ms" are the page's figures, while "5 ms" is the constant being
/// defined, "~25x" is arithmetic done in the comment, and "141 ms", "8" and
/// "256 KiB" are the page's too but round. Checking every number would have
/// reported the constant's own value and a ratio as unattributed on the
/// tree as it stood — three false positives against one true one, on the
/// very comment this exists for.
///
/// What that gives up, in order of how much it costs:
///
/// - **Integer figures.** "the page names 141 ms" is unchecked. This is the
///   real gap, and it is not small: a wrong round number is as believable
///   as a wrong precise one.
/// - **Attribution across paragraphs.** A figure a paragraph below the one
///   naming the document is unchecked. Doc comments break paragraphs at
///   headings, so a `# Where the number comes from` section that names the
///   page in its first paragraph and the figure in its second is missed.
/// - **Ordinary `//` comments and prose in `docs/src/**`.** Only `///` and
///   `//!` are read, in the four crates `SOURCE_CRATES` names.
/// - **Which figure means what.** A paragraph naming two documents passes
///   if the figure is in either, and a figure that appears in the document
///   in an unrelated sentence passes. This checks that the number is on the
///   page, not that the page says what the comment says it says.
/// - **Non-numeric attributions.** "the page forbids X" is prose (kind 3 in
///   the module header) and stays prose.
fn figure_attributions(root: &Path, crates: &[&str]) -> (usize, usize, Vec<Failure>) {
    let mut rs = Vec::new();
    for krate in crates {
        walk(&root.join(krate), SKIP_DIRS, &mut rs);
    }
    rs.retain(|p| p.extension().is_some_and(|e| e == "rs"));
    rs.retain(|p| !p.components().any(|c| c.as_os_str() == "citation_canary"));
    rs.sort();

    let doc_re = Regex::new(DOC_PATH_PATTERN).unwrap();
    let fig_re = Regex::new(FIGURE_PATTERN).unwrap();

    let mut index = Vec::new();
    walk(root, SKIP_DIRS, &mut index);
    let rel_index: Vec<String> = index
        .iter()
        .map(|p| {
            p.strip_prefix(root)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    let mut triggered = 0;
    let mut checked = 0;
    let mut failures = Vec::new();

    for para in doc_paragraphs(root, &rs) {
        let named: BTreeSet<String> = doc_re
            .find_iter(&para.text)
            .map(|m| m.as_str().to_string())
            .collect();
        if named.is_empty() {
            continue;
        }
        let figures = figures_in(&fig_re, &para.text);
        if figures.is_empty() {
            continue;
        }
        triggered += 1;

        let where_ = format!("{}:{}", para.file.display(), para.first_line);
        let mut bodies = Vec::new();
        let mut absent = Vec::new();
        for name in &named {
            let suffix = format!("/{name}");
            match rel_index
                .iter()
                .find(|f| *f == name || f.ends_with(&suffix))
            {
                Some(found) => bodies.push((
                    found.clone(),
                    fs::read_to_string(root.join(found)).unwrap_or_default(),
                )),
                None => absent.push(name.clone()),
            }
        }
        if !absent.is_empty() {
            // A doc comment attributing a figure to a document that is not
            // in the tree is the same defect one step further along: there
            // is nothing left to check the figure against.
            failures.push(Failure {
                kind: FailureKind::Missing,
                message: format!(
                    "{where_}\n    a figure is attributed to {}, which is not in the repo \
                     -- deleted or renamed?\n    figures in the paragraph: {}",
                    absent.join(", "),
                    figures.join(", ")
                ),
            });
            continue;
        }

        for figure in &figures {
            checked += 1;
            if bodies
                .iter()
                .any(|(_, body)| document_names_figure(&fig_re, body, figure))
            {
                continue;
            }
            let names: Vec<&str> = bodies.iter().map(|(n, _)| n.as_str()).collect();
            failures.push(Failure {
                kind: FailureKind::Drift,
                message: format!(
                    "{where_}\n    the figure {figure} is attributed to {}, which does not \
                     contain it.\n    Either the number is wrong, or it was never written down \
                     where the comment says it was --\n    a measurement that lives only in the \
                     comment claiming to quote it cannot be checked by anyone.\n    Put it on \
                     the page, or attribute it to where it actually is.\n    Paragraph: {}",
                    names.join(" or "),
                    para.text.chars().take(300).collect::<String>()
                ),
            });
        }
    }

    (triggered, checked, failures)
}

fn report(label: &str, failures: &[Failure]) {
    assert!(
        failures.is_empty(),
        "{} {label} citation(s) no longer point at what they claim.\n\
         Fix the citation, not the guard: each entry names the file and line, \
         the citation as written, and the nearest current match.\n\n{}",
        failures.len(),
        failures
            .iter()
            .map(|f| f.message.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}

// --- standing canary -----------------------------------------------------
//
// The concept page requires a permanent pair, not a one-time demonstration:
// a gate that stops matching -- a glob that no longer resolves, a parser that
// returns nothing -- is green forever, which is the defect the page exists to
// remove. The floor asserts below catch a parser that stops matching; this
// catches a checker that stops *failing*.
//
// The fixture is a miniature repo built in a tempdir and run through the same
// `scan` + `check` the real corpora use, so it exercises parse, resolve,
// window and classification end to end without polluting the corpus it
// guards. Both directions are asserted: the drifted, absent and missing
// citations must be reported, and the correct one must not.

/// Fixture bodies live in `tests/citation_canary/*.in`, not inline: a citation
/// spelled out in this file would be scanned as part of the corpus this file
/// guards. The fixture puts its subject at line `CANARY_SUBJECT_LINE` and pads
/// to 60 lines, so the citation to `CANARY_DRIFT_LINE` is inside the file but
/// far outside `WINDOW`. `tests/citation_canary/README.md` says what each of
/// the fixture citations is for.
const CANARY_TARGET: &str = include_str!("citation_canary/canary_target.rs.in");
const CANARY_CITATIONS: &str = include_str!("citation_canary/canary_citations.rs.in");
const CANARY_FIGURES: &str = include_str!("citation_canary/canary_figures.rs.in");
const CANARY_BUDGET: &str = include_str!("citation_canary/canary_budget.md.in");
const CANARY_SUBJECT_LINE: usize = 3;
const CANARY_DRIFT_LINE: usize = 50;

fn write_canary_fixture(root: &Path) {
    let src = root.join("leviculum-core/src/citation_canary");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("canary_target.rs"), CANARY_TARGET).unwrap();
    fs::write(src.join("canary_citations.rs"), CANARY_CITATIONS).unwrap();
}

/// The figure-attribution fixture. A separate tree from the one above: the
/// citation canary excludes `citation_canary` paths from its scan, and this
/// one needs its Rust file *inside* the corpus.
fn write_figure_canary_fixture(root: &Path) {
    let src = root.join("leviculum-core/src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("canary_figures.rs"), CANARY_FIGURES).unwrap();
    let docs = root.join("docs/src/concepts");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("canary_budget.md"), CANARY_BUDGET).unwrap();
}

/// Both directions, on the same fixture: the unsupported figure and the
/// missing page must be reported, and the supported figure, the version
/// string and the two integers in the same paragraph must not.
///
/// A one-time demonstration is not enough. A trigger that stops matching --
/// a paragraph splitter that returns nothing, a boundary rule that rejects
/// every figure -- reports zero findings forever, which reads exactly like
/// a clean tree. It is the shape this whole page exists to remove, and the
/// #200 case itself was fixed hours before the guard was written, so the
/// real corpus cannot supply the failing side.
fn run_figure_canary() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write_figure_canary_fixture(root);

    let (triggered, checked, failures) = figure_attributions(root, &["leviculum-core"]);
    // Two of the fixture's three paragraphs, exactly. The third names the
    // page and quotes only the version string `0.8.0`, which the boundary
    // rule must not read as the figure `0.8`. So this number is pinned in
    // both directions at once: 1 or 0 means the trigger stopped matching and
    // a green run means nothing, 3 means the boundary rule broke and every
    // version string in the tree is about to be reported as a figure.
    assert_eq!(
        triggered, 2,
        "CANARY: {triggered} of the fixture's 3 paragraphs triggered, expected 2."
    );
    // 3.2 and 126.6. The third paragraph's 1.5 is deliberately not among
    // them: its page is absent, and there is nothing to check a figure
    // against.
    assert_eq!(
        checked, 2,
        "CANARY: {checked} figures were checked, expected 3.2 and 126.6. \
         Figure extraction has stopped working."
    );

    let msgs: Vec<&str> = failures.iter().map(|f| f.message.as_str()).collect();
    let joined = msgs.join("\n\n");
    assert!(
        failures
            .iter()
            .any(|f| f.kind == FailureKind::Drift && f.message.contains("figure 126.6")),
        "CANARY: a figure attributed to a page that does not contain it was NOT \
         reported. This is the defect the check exists for.\n{joined}"
    );
    assert!(
        failures
            .iter()
            .any(|f| f.kind == FailureKind::Missing && f.message.contains("canary_gone.md")),
        "CANARY: an attribution to a document absent from the tree was not \
         reported; a renamed page would switch the check off silently.\n{joined}"
    );
    // Everything below is the false-positive side. A guard that reports these
    // gets switched off, and a switched-off guard is worse than none.
    for quiet in ["figure 3.2", "figure 0.8", "figure 5", "figure 25"] {
        assert!(
            !joined.contains(quiet),
            "CANARY: `{quiet}` was reported. 3.2 is on the page, 0.8 is half of \
             the version string 0.8.0, and 5 and 25 are integers the comment \
             derives rather than quotes.\n{joined}"
        );
    }
    assert_eq!(
        failures.len(),
        2,
        "CANARY: expected exactly the drift and the missing page; got {}:\n{joined}",
        failures.len()
    );
}

fn run_canary() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write_canary_fixture(root);

    let citations = scan(
        root,
        &{
            let mut v = Vec::new();
            walk(&root.join("leviculum-core"), SKIP_DIRS, &mut v);
            v.retain(|p| p.extension().is_some_and(|e| e == "rs"));
            v.sort();
            v
        },
        Corpus::Source,
    );
    assert_eq!(
        citations.len(),
        8,
        "CANARY: the parser found {} of 8 fixture citations. It has stopped \
         matching; every green run since it broke means nothing.",
        citations.len()
    );

    let (counts, failures) = check(root, &citations);
    // Six of the eight name what they point at: four in the paren spelling,
    // two in the comma spelling. The remaining two are the last fixture
    // line, whose leading backticked token is itself a citation -- pinned in
    // both directions at once, because 5 means the comma spelling stopped
    // being seen (and 130 real citations silently fell back to an existence
    // check) while 7 means a citation next to a citation is being read as
    // its subject.
    assert_eq!(
        counts.with_ident, 6,
        "CANARY: identifier detection saw {} of 6, which silently changes \
         how much of the corpus is drift-checked.",
        counts.with_ident
    );

    let kinds: Vec<&FailureKind> = failures.iter().map(|f| &f.kind).collect();
    assert_eq!(
        kinds.len(),
        4,
        "CANARY: expected exactly 4 failures (two drifts, missing, absent \
         submodule); got {}:\n{}",
        kinds.len(),
        failures
            .iter()
            .map(|f| f.message.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    // Built rather than spelled out: a literal citation in this file would be
    // scanned as part of the corpus this file guards.
    let drifted = format!("canary_target.rs:{CANARY_DRIFT_LINE}");
    let correct = format!("canary_target.rs:{CANARY_SUBJECT_LINE}");
    // The drifted citations must be reported: this is the failure the guard
    // exists for, and the one that decays silently. Once per spelling --
    // a comma-form drift that goes unreported is the `lora.rs:725` case
    // over again, which is what admitting the spelling was for.
    let drifts: Vec<&Failure> = failures
        .iter()
        .filter(|f| f.kind == FailureKind::Drift && f.message.contains(&drifted))
        .collect();
    assert_eq!(
        drifts.len(),
        2,
        "CANARY: {} of the 2 deliberately drifted citations (paren spelling, \
         comma spelling) were reported. The guard cannot see the defect it \
         exists to catch in one of the two forms the corpus writes.",
        drifts.len()
    );
    // The correct ones must not be, or the guard is noise and gets disabled.
    // `: <raw>\n` is how `check` opens a failure message, so this matches the
    // citation as written and not a line number quoted inside a note.
    assert!(
        !failures
            .iter()
            .any(|f| f.message.contains(&format!(": {correct}\n"))),
        "CANARY: a correct citation was reported as broken:\n{}",
        failures
            .iter()
            .map(|f| f.message.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    // Absent-submodule and drift must stay distinguishable: sending a reader
    // to `git submodule update --init` for a drifted citation, or to the
    // prose for an absent reference, is how the LXMF incident stayed open.
    let absent = failures
        .iter()
        .find(|f| f.kind == FailureKind::SubmoduleAbsent)
        .expect("CANARY: a citation into an unchecked-out submodule was not classified as absent");
    assert!(
        absent.message.contains("NOT a drifted citation"),
        "CANARY: the absent-submodule message no longer distinguishes itself \
         from a drift: {}",
        absent.message
    );
    assert!(
        failures
            .iter()
            .any(|f| f.kind == FailureKind::Missing && f.message.contains("canary_absent_file.rs")),
        "CANARY: a citation to a nonexistent file was not reported."
    );
}

#[test]
fn citation_guard_canary() {
    run_canary();
    run_figure_canary();
}

/// Codeberg #213: a citation into an extensionless hook under `.githooks/`
/// is seen, and nothing else became visible with it.
///
/// The guard's coverage of that directory used to be exactly zero — the
/// hooks have neither an extension nor a name the pattern knew, so the
/// best-written possible citation into one was skipped while the identical
/// citation one file over resolved. `post-commit` was then deleted with
/// seven references to it across five files and the guard ran green in the
/// same invocation.
///
/// Both directions are pinned, because the extension requirement that
/// caused the blind spot is also what keeps `127.0.0.1:4242` and Python
/// slices out, and a guard with false positives gets switched off.
#[test]
fn githook_citations_match_without_loosening_the_pattern() {
    let source = cite_regex(Corpus::Source);
    let book = cite_regex(Corpus::Book);

    // The leading dot is part of the captured path: the directory is spelled
    // `.githooks` on disk and `check` resolves the path as written, so a
    // capture that dropped it would resolve to nothing.
    let caps = source
        .captures("lints the pipelines, .githooks/pre-push:21, before Tier 0")
        .expect("a .githooks citation in a source comment must match");
    assert_eq!(&caps[1], ".githooks/pre-push");
    assert_eq!(&caps[2], "21");

    let caps = book
        .captures("the commit-msg hook (`.githooks/commit-msg:5`) does the same")
        .expect("a .githooks citation in the book must match");
    assert_eq!(&caps[1], ".githooks/commit-msg");

    // A citation into a hook that has been deleted must now be *matched*, so
    // that `check` can report it missing — that is the whole point, and the
    // half no green run can demonstrate. Built rather than spelled out: a
    // literal here would be scanned as part of the corpus this file guards
    // and would fail the guard it is testing.
    let deleted = format!(".githooks/{}-commit:10", "post");
    assert!(
        source.is_match(&deleted),
        "a citation into a deleted hook must be matched so it can be reported"
    );

    // The false-positive side. Extensionless *tokens* stay out; only paths
    // under an allowlisted directory come in.
    for prose in [
        "pre-push:21",
        "post-commit:10",
        "commit-msg:5",
        "127.0.0.1:4242",
        "packed[:16]",
        // A path is a citation only with a line spec; that has not changed.
        ".githooks/pre-push",
    ] {
        assert!(
            !source.is_match(prose),
            "`{prose}` must not be read as a citation -- the pattern has been \
             loosened into matching prose"
        );
    }

    // End to end through the real `scan` + `check`, on a fixture repo: a
    // hook citation that resolves must stay quiet, and one into a hook that
    // is not there must be reported missing.
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::create_dir_all(root.join(".githooks")).unwrap();
    fs::write(
        root.join(".githooks/pre-push"),
        "#!/bin/bash\nset -e\njust fast\n",
    )
    .unwrap();
    let docs = root.join("docs/src");
    fs::create_dir_all(&docs).unwrap();
    let gone = format!(".githooks/{}-commit:1", "post");
    fs::write(
        docs.join("hooks.md"),
        format!("Tier 0 runs from `.githooks/pre-push:3`, and once from `{gone}`.\n"),
    )
    .unwrap();

    let citations = scan(root, &[docs.join("hooks.md")], Corpus::Book);
    assert_eq!(
        citations.len(),
        2,
        "the fixture's two hook citations were not both parsed"
    );
    let (_, failures) = check(root, &citations);
    let messages: Vec<&str> = failures.iter().map(|f| f.message.as_str()).collect();
    assert_eq!(
        failures.len(),
        1,
        "expected exactly the citation into the absent hook: {}",
        messages.join("\n\n")
    );
    assert_eq!(failures[0].kind, FailureKind::Missing);
    assert!(
        failures[0].message.contains(&gone),
        "the reported failure names the wrong citation: {}",
        failures[0].message
    );
}

/// Both spellings that attach a name to a citation are seen, and nothing
/// else became a name with them.
///
/// The false-positive side is the load-bearing half. A token wrongly read
/// as the citation's subject does not usually produce a red — it produces a
/// citation counted as drift-checked whose needle (`3`, `0001`) matches
/// something within the window by accident. That is worse than leaving it
/// bare, because the coverage number then says the citation is checked.
#[test]
fn a_citation_names_its_subject_in_either_spelling_and_in_nothing_else() {
    let idents = ident_regexes();
    let shaped = citation_shaped();
    let ident = |before: &str| attached_ident(before, &idents, &shaped);

    // The two spellings, including across a line break.
    assert_eq!(
        ident("derived by `resolve_lt_alock` ("),
        Some("resolve_lt_alock".into())
    );
    assert_eq!(
        ident("derived by (`resolve_lt_alock`, "),
        Some("resolve_lt_alock".into())
    );
    assert_eq!(ident("(`erp_band_gap`,\n"), Some("erp_band_gap".into()));
    // The function spelling loses its parens; `Type::method` and
    // `module.attr` keep theirs, because `check` searches the last segment.
    assert_eq!(ident("`airtime_ms()` ("), Some("airtime_ms".into()));
    assert_eq!(
        ident("`RadioConfig::eu_medium` ("),
        Some("RadioConfig::eu_medium".into())
    );

    for not_attached in [
        // Prose between the name and the citation: the pairing has to be
        // unambiguous, so an identifier mentioned earlier in the sentence
        // is not the citation's subject.
        "`Transport.outbound()` at ",
        "`Transport.outbound()` is the loop, and (",
        // A citation next to a citation, the shape a comparison table has.
        "| Self-announce one-shot | `Destination.py:322`, ",
        "the recipe moved (`Justfile:719`, ",
        // A token with no letter in its last segment cannot be searched
        // for as an identifier.
        "the VID:PID `1209:0001` (",
        "`4.2` (",
        // Nothing at all: the bare citation, which stays existence-checked.
        "while the tracker is locked (",
    ] {
        assert_eq!(
            ident(not_attached),
            None,
            "`{not_attached}` was read as naming the citation that follows it"
        );
    }
}

/// Guarantee C, kind 2: a figure a doc comment attributes to a document
/// must occur in that document (Codeberg #200).
#[test]
fn doc_comment_figures_are_on_the_page_they_cite() {
    run_figure_canary();

    let root = repo_root();
    let (triggered, checked, failures) = figure_attributions(&root, SOURCE_CRATES);

    // Published like the citation counts above, and for the same reason: a
    // trigger this narrow finds very little, and the number it found is the
    // only thing that tells a reader whether "no failures" means the tree is
    // clean or the trigger stopped firing. The canary is the real guard
    // against the second; this is what makes it visible without one.
    println!(
        "figure attributions ({}): {triggered} paragraph(s) naming a docs/ page \
         and quoting a decimal, {checked} figure(s) checked",
        SOURCE_CRATES.join(", ")
    );

    report("figure-attribution", &failures);
}

#[test]
fn doc_citations_resolve() {
    run_canary();

    let root = repo_root();
    let citations = book_citations(&root);
    let (counts, failures) = check(&root, &citations);

    println!(
        "doc citations: {} total, {} identifier-checked, {} bare \
         (existence/length only), {} external (unchecked)",
        counts.total(),
        counts.with_ident,
        counts.bare,
        counts.external
    );
    report_offsets("doc", &counts);

    // Tripwire against parser rot, not a coverage target: the corpus has
    // ~800 citations (~70 with identifiers) as of 2026-08. A guard that
    // silently stops matching is worse than none; if the docs shrink
    // deliberately, lower these floors in the same commit.
    assert!(
        counts.total() >= 300,
        "only {} citations parsed -- parser rot?",
        counts.total()
    );
    assert!(
        counts.with_ident >= 30,
        "only {} identifier citations parsed -- parser rot?",
        counts.with_ident
    );

    report("doc", &failures);
}

#[test]
fn source_citations_resolve() {
    run_canary();

    let root = repo_root();
    let citations = source_citations(&root, SOURCE_CRATES);
    let (counts, failures) = check(&root, &citations);

    // Published on every run, like the book's, so nobody reads a green guard
    // as full coverage. The bare majority is real: for those citations this
    // is existence-and-length checking, which catches renames and deletions
    // and not drift inside a file that stays long enough. Converting them to
    // the ``ident` (`path:line`)` form is editorial work (#167).
    println!(
        "source citations ({}): {} total, {} identifier-checked, {} bare \
         (existence/length only), {} external (unchecked)",
        SOURCE_CRATES.join(", "),
        counts.total(),
        counts.with_ident,
        counts.bare,
        counts.external
    );
    report_offsets("source", &counts);

    // Same tripwire role as the book floors above.
    assert!(
        counts.total() >= 500,
        "only {} source citations parsed -- parser rot?",
        counts.total()
    );
    assert!(
        counts.with_ident >= 30,
        "only {} identifier source citations parsed -- parser rot?",
        counts.with_ident
    );

    report("source", &failures);
}

#[test]
fn concept_docs_reachable_from_summary() {
    let root = repo_root();
    let summary_path = root.join("docs/src/SUMMARY.md");
    let summary = fs::read_to_string(&summary_path).unwrap();
    let link_re = Regex::new(r"\]\(([^)]+\.md)\)").unwrap();

    let linked: BTreeSet<String> = link_re
        .captures_iter(&summary)
        .map(|c| c[1].to_string())
        .collect();

    // Every SUMMARY entry must point at an existing file: a dangling
    // entry renders as an empty chapter.
    let dangling: Vec<&String> = linked
        .iter()
        .filter(|l| !root.join("docs/src").join(l.as_str()).is_file())
        .collect();
    assert!(
        dangling.is_empty(),
        "SUMMARY.md links without a file behind them: {dangling:?}"
    );

    // Every concept document must be in SUMMARY.md: an unlisted file is
    // invisible in the built book, and an invisible policy document is
    // policy nobody reads.
    let on_disk: BTreeSet<String> = fs::read_dir(root.join("docs/src/concepts"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
        .map(|e| format!("concepts/{}", e.file_name().to_string_lossy()))
        .collect();
    let listed: BTreeSet<String> = linked
        .iter()
        .filter(|l| l.starts_with("concepts/"))
        .cloned()
        .collect();

    let orphaned: Vec<&String> = on_disk.difference(&listed).collect();
    assert!(
        orphaned.is_empty(),
        "concept document(s) not listed in docs/src/SUMMARY.md \
         (invisible in the built book): {orphaned:?}"
    );
}
