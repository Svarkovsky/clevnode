Fixture bodies for the standing canary in `leviculum-std/tests/doc_citations.rs`.

They are the guard's own input, not part of the corpus it guards, and are
excluded from the source scan by path component. `canary_citations.rs.in`
holds eight citations into `canary_target.rs.in`, on seven lines:

- in the paren spelling ``ident` (`path:line`)`: one correct, one drifted
  past the identifier window, one to a file that does not exist, and one
  into a `reference/` submodule that is absent from the fixture tree;
- in the comma spelling `(`ident`, `path:line`)`: one correct and one
  drifted, so that a spelling admitted to the guard cannot later stop
  being seen while the corpus keeps writing it;
- one line whose leading backticked token is itself a citation. It must
  NOT be read as the following citation's subject — both citations on that
  line are bare, and both pass on existence alone.

The guard must report exactly four failures: the two drifts, the missing
file, and the absent submodule — the last classified as absent rather than
drifted. Six of the eight citations must be counted as naming a subject.

`canary_figures.rs.in` and `canary_budget.md.in` are the pair for the
figure-attribution check (Codeberg #200). The Rust file holds three
doc-comment paragraphs: one attributing 3.2 ms (present) and 126.6 ms
(absent) to the page, spelled the way the real defect was written, with the
attribution one sentence and the unsupported figure two sentences later;
one quoting the version string `0.8.0`, which must not be read as the
figure `0.8`; and one attributing a figure to a page that is not in the
fixture tree. The guard must report exactly two things: the drifted 126.6
and the missing page. It must not report 3.2, must not report the version,
and must not report `5` or `25` — the constant's own value and a ratio,
both integers, both in the same paragraph as the attribution.

Editing any of these files changes what the canary proves. If a change
makes the canary fail, the guard is what needs looking at, not the fixture.
