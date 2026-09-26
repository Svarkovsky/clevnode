# Contributing

For a contributor about to open a PR: the one rule, what a change must
pass, and how commits are attributed.

## A check is not implemented until it has been observed failing

A guard or assertion counts as done when someone has watched it fail —
until then it is indistinguishable from nothing, because nothing also
passes. Write the failing case first, or extract the decision into a
pure function so the failing side is reachable without hardware, and
make the test measure what production reads. The mechanics behind that
rule — every pin carrying a negative control, every test being executed
by some gate, every citation still pointing at what it claims — are in
[Checks That Are Actually Checks](docs/src/concepts/checks-and-citations.md);
the rule itself is in
[Evidence and Honesty](docs/src/concepts/evidence-and-honesty.md).

## What a change must pass

`just fast` on every commit (fmt, clippy with `-D warnings`, the
workspace lib tests, and the submodule, citation and commit-trailer
gates), `just standard` before pushing anything that touches a core
type. Protocol- or daemon-touching changes add `periculum run
conformance`, every scenario green — the corpus grows, so `periculum
list` prints the live count rather than this file. See
[Testing](docs/src/development-testing.md) for the tiers and what each
one owes.

Do not commit while tests are red, and do not carry a red test forward
as a known issue.

## Commit messages and attribution

Documentation and commit messages are English; no AI trailers. Commit
under your real name and a reachable e-mail address; sign-off is not
required.

The wording above is [periculum's](https://codeberg.org/Lew_Palm/periculum),
verbatim and on purpose: the two repositories are developed together and
a rule that is stated once and relied on twice is a rule with a seam in
it. It had that seam until 2026-08-07, when a `Co-Authored-By:` naming a
model reached a periculum commit — in a pass whose author had cited this
exact rule, correctly, hours earlier — and was caught only because a
human read the message before pushing.

So the rule now has something behind it in both repositories:

* `scripts/check-commit-trailers.sh` rejects a commit message carrying a
  machine-authorship line. It matches only at column 0, because that is
  where a trailer is a trailer; a message that needs to *quote* one
  indents it.
* `.woodpecker/commit-trailers.yml` runs that over every commit pushed
  since a pinned baseline. This is the enforcement, and it is a forge
  check rather than a hook because a fresh clone has no hooks.
* `.githooks/commit-msg` — the only commit-msg hook in the tree, and the
  one `scripts/install-ci.sh` installs by pointing `core.hooksPath` at
  `.githooks` — runs the same script at commit time so the failure
  arrives while the message is still in the editor. It refuses; it never
  edits what you wrote. Convenience, not enforcement.

**The rule binds our commits, not yours.** A machine-authorship trailer
is a violation on a commit authored by one of the project's own
identities — the `ours` lines in `scripts/commit-trailer-baseline.txt` —
and on nothing else: we do not rewrite, and do not refuse, a message
somebody outside the project wrote. Your attribution is yours. The hook
and the pushed-range check both read `git log --format=%ae` and agree;
if you install our hooks in your own clone they will not ask you to
change your commit messages.

That exemption is counted rather than trusted. `foreign` in the same
file pins how many commits above the baseline carry such a trailer under
an author that is not ours, and the check recomputes it every run — so a
new external contribution with a trailer lands, keeps its message, and
still shows up as a number moving in a diff. Left uncounted, the
exemption would be an off switch anyone could reach by setting an author
e-mail.

Sixteen commits below the baseline carry such a trailer. They are listed
in the same file rather than rewritten out of published history, and
their count is checked the same way, so the baseline cannot be quietly
moved forward.

## Licensing

Contributions are AGPL-3.0-or-later; see [LICENSE](LICENSE).
