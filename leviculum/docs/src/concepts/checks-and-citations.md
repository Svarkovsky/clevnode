# Checks That Are Actually Checks

A check makes two promises: that it *ran*, and that it *could have
failed*. A citation makes a third: that it still points at what it
claims. None is self-evident, and this codebase has broken all three —
silently, and for months at a time.

This page records the three mechanical guarantees that make those
promises verifiable, what they cost, and what they do not reach.

The underlying rule is in [Evidence and Honesty](evidence-and-honesty.md):
*a check you have never seen fail is not a check*. That page tells a
person what to do. This one is about the cases where the person forgot.

## The incidents, and which guarantee would have caught each

| Defect | Caught by |
|---|---|
| `execute_benchmark` ended in an unconditional `Ok(())` (`periculum/src/assertions.rs`); 6 of 10 recorded runs carried zero packets and reported GREEN | **neither** — a scenario step, not a Rust test |
| the sx1262 RX-extend guard tested IRQ flags its own latch mask had disabled (#144, fixed 26ce3a0) | **neither** — no test at all, and `exclude` (`Cargo.toml:39`) puts `leviculum-nrf` outside the workspace |
| `status_parity` `#[ignore]`d with a reason naming a procedure no script implements; never executed by any gate (#189) | **B** |
| 14 further ignored tests in `rnsd_interop` executed by nothing (#189) | **B** |
| scenario steps across the corpus produced a delivery figure no step asserted; GREEN at 70-90 % (#188, Periculum #25) | **neither** — scenario steps |
| the status-parity volume guard compared one interface, so a whole-inventory divergence stayed green (#177) | **A**, only if the author's negative control covers the whole inventory rather than the one interface they compared |
| drifted `file:line` citations — six across five concept documents in the 2026-07 manual audit (`leviculum-std/tests/doc_citations.rs:6`), sixteen across the whole book on the guard's first automated run | **C** |
| `reference/LXMF` sat twelve commits behind its gitlink for five weeks; every LXMF citation meant something other than it said | **C**, and the red `reference_lock` test that should have said so was itself unobserved — a **B** failure masking a **C** failure |
| a `Co-Authored-By:` naming a model reached a periculum commit on 2026-08-07, against a rule the same author had cited correctly hours earlier (#205) | **neither** — a commit message, which all three explicitly do not reach |
| `PROCESSOR_TICK_BUDGET` justified the only number in a public API constant with "the number comes off `docs/…/core-lock-budget.md`" and then named 126.6 ms; that figure occurred exactly once in the tree, in that comment (#200) | **C**, only since the figure check below — a prose attribution carries no line and no identifier, so the resolver never saw it |
| `just standard` held a decided red for two hours, alive and silent, because a test that aborted in a destructor leaked a daemon holding the gate's stdout pipe (2026-08-07) | **none of the three** — every one of them reports, and a gate that never terminates reports nothing at all. See [A gate must pass, fail, or say it gave up](#a-gate-must-pass-fail-or-say-it-gave-up) |
| seven orphaned `scripts/test_daemon.py` processes alive at once on 2026-08-07, the oldest over four hours, from several different runs — every one of them left by a test whose `Drop` was written correctly and did not run | **none of the three**, and nothing else either: an orphan makes no gate red, so the only thing that ever reported it was somebody running `pgrep` by hand. Now **B**, via the census and the SIGKILL proof under [A harness that spawns a process must ensure it dies with the harness](#a-harness-that-spawns-a-process-must-ensure-it-dies-with-the-harness) |

The last row is worth reading twice: the guarantees are not
independent. A rotted citation had a test attached, and that test ran
nowhere. **Guarantees that only report are worth what their observation
is worth.**

## Guarantee A — every pin carries its own negative control

A **pin** is a test that fixes a claim we rely on: a wire-field
semantic, a deliberate deviation, a measured protection level, a chosen
non-behaviour.

The rule: **a pin must contain an assertion that fails when the claim it
pins is broken**, in the same test, on every run. The pattern is in the
tree — `announce_signature_covers_reference_byte_order_on_the_wire`
(`leviculum-core/src/destination.rs:2266`) verifies its signature, then
drops the destination hash from the front and asserts that verification
now fails.

### The gate checks presence. Only the audit checks efficacy.

A registry gives the gate a citation. The strongest thing it can check
is that the cited assertion still exists where it says — drift
detection. **It cannot see a vacuous control**: one that verifies
against a random key, one behind an early return, one in a branch the
test never takes. Efficacy is checkable only by mutating the pin's
subject and confirming the pin dies.

**A is therefore not in force until the audit runs.** The registry gate
will land first because it is cheap; a tree that has only the registry
has drift detection on negative controls and nothing more, and should
not be described as having Guarantee A.

### Why mutation cannot be the per-batch gate

`-j` used to be unsafe here, and ports were the reason. Storage never
was: the suites take theirs from `tempfile::tempdir()`, and
`cargo-mutants` gives each job its own copy of the tree anyway. Both
the mvr and `rnsd_interop` suites drew listeners from a counter over a
fixed band, 61000-65000, and that counter was per-process: each test
binary started at the same base and walked the same numbers, so two
concurrent processes raced in the alloc → bind handoff window. Measured
on two concurrent runs of the mvr binary under `strace -e trace=bind`:
10 ports bound by both processes and ~80 `EADDRINUSE` binds in the band
per run, none of which went red, because the probe loop retried.

That is fixed. `next_port_candidate`
(`leviculum-std/tests/support/port_alloc.rs`) draws from one counter per
*host*, kept in a file and bumped under `flock`, so no two processes are
handed the same number — the same measurement after the change reports
0 and 0. `tests/port_alloc_multiprocess.rs` pins it with four concurrent
worker processes and a negative control that must collide.

What is left is cost, not correctness: cold baseline builds and
hang-mutant timeouts, and the rebuild term below.

Measured on a 32-core host (musl, warm; the CI host has four cores,
where every figure below is worse): incremental rebuild after a content
change in `leviculum-core/src/transport.rs` ~1.65 s; the downstream
`leviculum-std --test mvr` binary ~1.8 s; `leviculum-core --lib`
build-and-run 4.3 s. Floor ~2 s per mutant. `cargo-mutants` generates
roughly 3-15 mutants per subject (its documented behaviour, not measured
here). At an *assumed* 200 pins that is 600-3000 mutants:
**realistically over an hour**, against a ~15 min batch budget. The
dominant term is the rebuild, which scales with the codebase, not with
the number of pins.

Two reach limits. `cargo-mutants` replaces a function body with a
guessed value, swaps binary operators, deletes unary ones, deletes match
arms a wildcard still covers, replaces match guards with `true` and
`false`, and deletes fields from struct literals that have a base
expression. It does not substitute literals and does not mutate consts —
where this project's semantics live: `PATHFINDER_RETRIES`
(`leviculum-core/src/constants.rs:115`). The #192 defect was
`retries: 0` where `PATHFINDER_RETRIES` belonged, and the fixed site
writes `PATHFINDER_RETRIES` (`leviculum-core/src/transport.rs:8653`)
into a literal that spells every field out — so not even the
field-deletion operator reaches it, and no operator substitutes one
const for another.

### What is a pin

Marked in the test by a doc-comment tag, **and** listed in
`scripts/pins.txt` with the location of its negative control. The gate
checks both directions: every tag has an entry and every entry has a
tag. Registration alone would make pinhood circular — the gate would
check that everything in the registry is in the registry, and a
pin-worthy test nobody registers would be silently absent.

The location is checked by reusing the window rule from
`leviculum-std/tests/doc_citations.rs`, so a moved control is caught the
same way a moved doc citation is. That is Guarantee C doing A's work,
which is the point of having all three on one page.

## Guarantee B — every test is executed by some gate

Half of it exists: `scripts/check-ignored-counts.py` enumerates tests
per unit and pins the ignored count. That stops the bucket growing
silently; it says nothing about whether anything in the bucket runs.

The other half:

1. **Every gate emits a manifest of what it executed**, by test name,
   per binary, **derived from run output, not from `cargo test --list`**
   — a list records intent. The hazard this must catch is a by-name
   selector that matches nothing: `cargo test <filter>` runs zero tests
   and exits 0, so a gate that selects by name reads green whether or
   not it measured anything. `scripts/run-status-parity.sh` already
   closes that hole by hand, pinning `EXPECTED=3` and parsing the
   summary line back — which is one gate's worth of what a manifest
   gives every gate.
2. **A check reports every test that exists and appears in no manifest**,
   by name.

### Enumerate at runtime, pin only counts

Item 2 must cover **all** tests, not only pins and declared exceptions.
The 14 unrun `rnsd_interop` tests were ordinary tests; a report scoped
to pins would not have seen them, and the per-unit count would have
stayed right the whole time — which is the argument against counts,
reintroduced.

The existing set is enumerated at run time, the way
`check-ignored-counts.py` already does it. Nothing needs a checked-in
list of ~3600 names, which would invite a `--bless` flag that silently
blesses the deletion it exists to catch.

What *is* pinned is **counts per unit**, and only for deletion
detection: a test that vanishes leaves both the manifest and the
runtime enumeration, so only a baseline catches it. Counts suffice for
that, and the file stays the size of `ignored-counts.txt`.

Note the per-configuration caveat: `leviculum-ffi` is gnu-only and
outside `default-members`, `leviculum-nrf` outside the workspace, so the
canonical configuration for the counts must be named.

Rollout has an ordering constraint: counts and reporting cannot be
enforced until every gate emits manifests, or everything reads as unrun.
Manifests first, enforcement second.

Step 1 is built: `scripts/run-with-manifest.py` wraps a gate's test
command, parses the names out of the run and writes one JSON manifest per
gate under `~/.local/state/leviculum-ci/test-manifests/`, next to the
other CI run state rather than in the tree or in `target/` — the tiers
run with their own `CARGO_TARGET_DIR`, so a manifest under `target/`
would split the union into one per tier.

Coverage was the problem the manifests then made visible: fourteen gates
emitted, and their union covered 3361 of the 3719 tests the workspace
held. The 358 outside it were 31 `#[ignore]`d and **327 ordinary tests
executed by no gate at all** — `leviculum-cli` and `leviculum-micron`
entirely, most of `leviculum-lxmf`, the `jl`/`jldiff` suites and every
`lnomad` test (#194).

Naming the missing 327 is what produced the gap: naming is a list, and a
list loses something again with every new test file. The fix inverts it.
`just complete`, in the `extensive` tier, runs
`cargo test --workspace --all-targets --no-fail-fast` and
`cargo test --workspace --doc --no-fail-fast`, selects nothing by name,
and therefore covers everything by construction. **Tiers define latency,
not coverage**, and leaving the complete run is what has to be declared.

Sixteen gates emit today and the union covers 3690 of 3721; the 31
outside it are `#[ignore]`d, and no ordinary test is outside it.

Two spellings matter and neither is optional. `--all-targets` makes cargo
drop doctests, so `--doc` is a second invocation. Without `--no-fail-fast`
cargo stops after the first red binary, and the manifest would then record
a prefix of the workspace while reading like the whole of it.

Step 2 must normalise libtest's run-time name suffixes before it can
report anything: a `no_run` doctest is listed plainly and *run* as
`<name> - compile`, exactly as a `#[should_panic]` test is run as
`<name> - should panic`. Six doctests read as uncovered in the first
measurement for that reason alone, having in fact executed.

### Declared exceptions expire by execution, not by date

A test may legitimately run in no automatic gate: a rig test needing
hardware, a soak run before a release. The mechanism does not forbid it;
it requires the fact to be declared, with who runs it and where.

An expiry *date* is gateable but is the wrong quantity — it goes red on
a day unrelated to any change, and the cheapest fix is bumping it a
year, a one-line diff indistinguishable from maintenance. Instead: **an
exception is stale when no manifest has recorded that test executing for
longer than N.** It uses the manifests already being built, cannot be
satisfied by editing a number, and folds the exception list into the
staleness bound rather than leaving it a separate off switch.

The same bound applies to manifests themselves: a union with no age
limit counts a gate retired weeks ago.

The repo tried exactly this bound for tier 2, and how it failed is worth
recording, because the failure was not in the idea. `pre-push` blocked
when the newest `tier2 GREEN` line in the CI ledger was over 24 hours or
10 commits old. The only writer of that line was
`scripts/run-tier2.sh`; the timer that started it was retired on
2026-06-12; and the remedy the block printed — `just extensive` — drives
periculum directly and writes no line at all. So the bound became
unsatisfiable on the day the timer went, and stayed that way for 46 days
and 502 commits, every one of which reached master through
`--no-verify`. That flag disables the *whole* hook, lint and Tier 0 and
the trailer guard with it. The block was removed on 2026-08-07 rather
than repaired.

**A staleness bound measures its writer, not its subject.** Age a
manifest out only against a signal that something is still scheduled to
emit, and let the remedy name that emitter rather than a recipe which
merely looks equivalent. A bound whose remedy cannot clear it does not
fail open or closed — it fails into `--no-verify`, and takes the checks
that worked with it.

Two reach limits: tier-3 hardware manifests and the release-only
hardware corpus (`periculum list hardware` prints the live count; it
grows) are produced on another host on a per-release cadence, so
either their transport is specified or the guarantee is scoped to
host-runnable gates. And `nextest` cannot run doctests, of
which `scripts/ignored-counts.txt` tracks two units (`leviculum-core
--doc` and `leviculum-std --doc`) — whichever runner is used, the
doctest gap is explicit.

## Guarantee C — every citation still points at what it claims

Our method rests on citations: a pinned deviation means nothing without
the reference line it deviates from. When a citation rots, the test
still passes and the sentence still reads — it simply stops being true.

`leviculum-std/tests/doc_citations.rs` is the working exemplar and the
proof that this rots fast. The *practice* of checking the reference
before auditing against it is already stated in
[Wire Field Semantics](wire-field-semantics.md); what follows is the
mechanism, not a restatement of the method.

### 1. The submodule check is O(1), and that is the whole incident

The five-week LXMF drift was not hundreds of citations going wrong
individually. It was **one fact**: the checked-out submodule disagreed
with its gitlink. The cheap catch is a per-batch assertion that
`git submodule status` reports no `+`/`-` for the four vendored
references — a few lines of shell, in a gate rather than in a `#[test]`
that Guarantee B can fail to observe.

That, and nothing more, would have caught the incident on day one.

### 2. Re-verification on a bump is a separate, larger problem

Binding every citation to a submodule commit and failing until they are
re-verified is a different mechanism, and it did not cause the incident.
If it is built, it must be incremental or it will not be used: on a
bump, `git diff --name-only old..new` inside the submodule bounds the
work to citations into changed files — usually a handful, not hundreds.

**This is what makes gap 3 a prerequisite rather than an afterthought.**
A citation written as ``ident` (`path:line`)` can be re-located
mechanically at the new commit and its line updated. A bare
`Transport.py:2970` can only be re-verified by a person reading it. So
converting *reference* citations to the identifier-adjacent form is what
turns a submodule bump from hundreds of manual re-reads into a
mechanical re-resolve.

### 3. Coverage: source is uncovered, and most citations are weak

The guard reads `docs/src/**`. A grep for reference citations
(`reference/…`, `Transport.py:`, `LXMRouter.py:`, `Destination.py:`)
across `leviculum-core`, `leviculum-lxmf` and `leviculum-std` finds
**421 in their `src/` trees, 497 counting their `tests/` directories —
against 804 covered in the book** — and the uncovered ones are
load-bearing, because they are what a pinned deviation cites.

Extending the glob to Rust source is cheap and should be done, but be
clear what it buys: for a bare citation it is existence-and-length
checking, so it catches renames and deletions and not drift inside a
file that stays long enough.

Counts from the guard's own output at the commit that landed this page:
**804 total, 72 identifier-checked, 731 bare, 1 external.** The bare
majority is an editorial problem — the fix is to write citations in the
identifier-adjacent form, and it cannot be mechanised without rewriting
prose. The honest response is to publish the ratio on every run so
nobody reads a green guard as full coverage, and to convert
opportunistically (#167 is the standing example).

### 4. A prose attribution is not a citation, and was not checked

`leviculum-std/tests/doc_citations.rs` resolves `path:line` and checks
identifier proximity. A sentence that attributes a number to a document
by name carries neither, so it sailed through — and that is not a rare
shape. `PROCESSOR_TICK_BUDGET`
(`leviculum-std/src/driver/processor.rs:172`) was justified with "The
number comes off `docs/src/concepts/core-lock-budget.md`" and then named
126.6 ms. The figure occurred exactly once in the whole tree: in that
comment. The measurement was real — taken in the #196 design pass — but
the page it was attributed to did not contain it, so the only number
behind a public constant could not be traced by anyone but its author.

The check: **a doc-comment paragraph naming a document under `docs/` and
quoting a decimal figure must have that figure occur in that document.**
`figure_attributions` in the same file, with the reach limits written
where a reader hits them.

Two decisions are worth lifting out of the code.

**Paragraph scope, not sentence.** The defect attributed across a
sentence boundary — the document named in one sentence, "The failure
mode it names is 126.6 ms" two sentences later — so a sentence-scoped
trigger would have missed the case it exists for. Reconstructed and run:
it is reported at paragraph scope and invisible at sentence scope.

**Decimal figures only, which is where the precision comes from.** In
the paragraph the defect lived in, "126.6 ms", "3.2 ms" and "0.8 ms" are
the page's figures; "5 ms" is the constant being defined and "~25x" is
arithmetic done in the comment. Checking every number would have
reported three of its own numbers alongside the one real finding — on
the very comment the check exists for. What it gives up is integers:
"the page names 141 ms" is unchecked, and a wrong round number is as
believable as a wrong precise one. That is the largest known gap and it
is stated rather than closed, because a guard with false positives gets
switched off, and a switched-off guard is worse than none.

Two paragraphs in the tree trigger it and five figures are checked. Both
numbers are printed on every run for the same reason the citation counts
are: with a trigger this narrow, "no failures" and "the trigger stopped
firing" are otherwise the same output.

### What C cannot reach

Issue comments, commit messages and batch reports carry hundreds of
`file:line` claims that nothing checks — and given how much of this
project's reasoning is recorded there rather than in the tree, that is
the largest uncovered surface of the three guarantees. A `just cite`
helper that emits a verified citation would reduce fabrication at the
point of writing; nothing can verify it after the fact.

### One line shape on that surface, and only one

`scripts/check-commit-trailers.sh` is the first mechanical check on a
commit message in either repository. It is worth being exact about how
little it does: it checks **one line shape**, not one claim. A message
may cite a file that does not exist, attribute a measurement to a page
that never carried it, and describe a fix it did not make; none of that
is reachable from here, and the paragraph above still stands whole.

What it does reach is #205, which was not a claim going wrong but a rule
losing to a default. `periculum/CONTRIBUTING.md:43` said "no AI
trailers. Commit under your real name and a reachable e-mail" while the
assistant harnesses used here instruct their agents to append exactly
that trailer to every commit. A rule in that position, with nothing
behind it, is not half-remembered — it is *reliably* broken, and its
violation is invisible unless a person reads every message before every
push. Which is how the one on 2026-08-07 was caught, and is not a
mechanism.

The check is a forge step
(`.woodpecker/commit-trailers.yml`, and the `commit-trailers` step in
periculum's `.woodpecker.yml`) over every commit since a pinned
baseline. `.githooks/commit-msg` runs the same script at commit time and
`just fast` runs it over the outgoing range, but neither is the
enforcement: a fresh clone has no hooks, which is the whole reason the
gate is where it is.

**It matches only at column 0.** That is not a compromise, it is the
definition: column 0 is where git's `interpret-trailers` and every forge
harvest a trailer, so it is where the default writes and the only place
the line is doing anything. It also leaves the one escape a message
sometimes needs — this guard's own commit message quotes the offending
trailer — namely the indentation git already uses for quoted material.
The alternative was git's own trailer block, the last paragraph; that
was rejected because the default's `Generated with <tool>` line sits in
its own paragraph *above* it, so the rule would have covered half the
default while reading as covering all of it.

A leading space defeats the check. That is a bound, not a hole: this
stands against a tool's default, and nothing message-shaped stands
against a person who has decided to misattribute authorship.

Sixteen commits below leviculum's baseline carry such a trailer, from
before the rule had anything behind it. They are recorded in
`scripts/commit-trailer-baseline.txt` rather than rewritten out of
published history, and **their count is recomputed and compared on every
run** — which is what stops the baseline being the off switch the expiry
dates above are criticised for being. Moving it forward to silence a
fresh failure moves a violation across that line and changes the number.

### The rule was narrower than the check, and the gap cost a day

As first written the guard scanned by message text alone. The rule the
repositories actually hold is narrower: **a machine-authorship trailer is
a violation on our commits and not on anyone else's** — we do not edit,
and do not refuse, a message somebody outside the project wrote. The
commit-msg hook in Lew's checkout had keyed on the author e-mail since
2026-06-13 and said so in a comment; the guard did not, and could not,
because nothing in the tree stated the policy. Merging PR #201, an
external contribution whose commit carries such a trailer, then turned
`just fast`, the pre-push hook and the forge check permanently red on a
tree with nothing wrong with it. The brief for #205 argued the rule
entirely in terms of our own harness default and never mentioned external
contributors; the guard did exactly what it was told.

This is the failure mode the page's opening promise misses. A check that
*could* have failed and *did* run can still be red for a reason the rule
does not hold — and a gate that is red on a clean tree gets switched off
or bypassed, which costs more than the gate was ever worth. **A check
also has to be able to go green on every tree the rule permits.**

Authorship is the discriminator and `git log --format=%ae` is the whole
mechanism. Two things follow, both of which are this page's own
arguments applied one level down:

- The exemption is **counted, not trusted**. `foreign` in the baseline
  file pins how many commits above the baseline carry a trailer under an
  author that is not ours, recomputed every run. Uncounted, an exemption
  granted by class is an off switch anyone can reach by setting an author
  e-mail; counted, a new external contribution lands with its message
  intact and still moves a number in a diff, which is the outcome we
  wanted — we want to know when it happens, we just do not want to
  rewrite somebody else's message.
- Who counts as ours is **a list in the reviewed file, not a constant in
  the script**. It has to be a set: `lp@lew-palm.de` is what we use at a
  terminal, but Codeberg stamps a web-UI edit with its own noreply
  address, and three commits in leviculum's history carry it. A
  single-address discriminator would have handed the foreign exemption to
  the project lead's own commits, silently — the exact failure the guard
  exists to prevent, arriving by accident rather than by intent. There is
  no computed control on that list, because who is inside the project is a
  declaration and not a fact the script can derive; the control is that
  the list sits next to the counts, where adding a line is a diff.

## A gate must pass, fail, or say it gave up

There is a fourth way for a gate to end, and it is worse than any red:
still running, verdict already determined, nobody told. On 2026-08-07
`just standard` sat for **two hours** in exactly that state. It was found
by noticing that its log's mtime was two hours old.

The mechanism, end to end:

1. `python_accepts_ratcheted_c_announce` (`leviculum-ffi`, `--test
   ffi_interop`) panicked **in a destructor during cleanup** — "thread
   caused non-unwinding panic. aborting." — and took SIGABRT.
2. An abort skips unwinding, so the `Drop` that would have killed the
   `scripts/test_daemon.py` that test had spawned never ran.
3. The orphaned daemon had inherited cargo's stdout. Confirmed by walking
   `/proc/*/fd`: PID 960389 held fd 2 on the same pipe.
4. `cargo` exited and became a zombie.
5. The wrapper read `for raw in proc.stdout:`. **That loop ends on EOF of
   the pipe, not on exit of the child.** `proc.wait()` sat behind it and
   was never reached. One surviving write end held the gate open.

Killing the orphan by hand finished the run *immediately*, printing the
`exit code 101` it had held for two hours.

The class is wider than the test that triggered it: **a gate that waits
for a pipe to close instead of for its child to exit can be held open by
any leaked grandchild.** Every harness here that spawns an external
process is exposed — the FFI tests spawn Python daemons and C binaries,
the interop tests spawn `lnsd` and `rnsd`, others spawn docker. So the
fix belongs in the wrapper, which is one place, and not in each spawn
site, which is many and will grow.

Three properties, in `scripts/run-with-manifest.py`:

1. **The verdict waits on the child, not on the pipe.** `proc.wait()`
   runs on the main thread and a reader thread drains stdout, so nothing
   the wrapper decides depends on EOF ever arriving. This is the property
   that would have ended the two hours by itself, and the only one that
   still holds when the other two fail.
2. **Orphans die with the gate.** The child is spawned with
   `start_new_session=True`, so it leads its own process group, and the
   whole group is killed once the child has exited — SIGTERM, then
   SIGKILL after a short grace, because what leaks here is daemons
   holding sockets, tempdirs and sometimes a serial port. The pipe then
   closes on its own and the tail of the output is drained normally. The
   cost on a clean run is one `/proc` scan that finds nothing.
3. **A hard timeout that reports.** Default 1800 s per wrapped command,
   overridable with `--timeout` per gate and `LEVICULUM_GATE_TIMEOUT`
   globally. On expiry the gate exits 124 — `timeout(1)`'s code — naming
   itself, how long it waited and what was still alive. The per-line
   flush that already existed makes the partial log true for free.

And the reporting rule, which is not decoration: **if the gate had to
kill survivors, it says so, by pid and command line.** A leaked daemon is
a bug in the test that leaked it, and a gate that cleans up silently
hides the bug it just worked around. The manifest carries the same facts
(`timed_out`, `killed`, `survived_sigkill`) so a nightly that kept only
the JSON can still see it.

What none of this reaches: an orphan that calls `setsid()` has left the
group and survives the kill. It cannot hold the gate open — property 1
does not care — but it is still alive afterwards, and the reader thread
has to be abandoned with the tail of the log unwritten. Both facts are
printed rather than swallowed. Interrupting the wrapper is also no longer
free: `start_new_session` detaches the child from the terminal's
foreground group, so INT/TERM/HUP are relayed by hand, or a Ctrl-C would
trade the hang at the end for an escape at the start.

### A harness that spawns a process must ensure it dies with the harness

The wrapper is a backstop, not an excuse. The rule for the spawn sites:

> **A harness that spawns a long-lived external process must ensure it
> dies with the harness, however the harness dies. Cleanup code is a
> convenience; the kernel is the guarantee.**

Relying on Rust's `Drop` to kill a child satisfies the "cleanly" half and
nothing else: an abort skips unwinding, and so does a SIGKILL of the test
binary. The Linux answer is `PR_SET_PDEATHSIG` on the child, set after
the fork and before the exec, so the kernel signals it when its parent
dies for any reason. `Drop` then becomes the polite path rather than the
only one.

The receipt, from the afternoon this was written: **seven orphaned
`scripts/test_daemon.py` processes alive at once, the oldest over four
hours, from several different runs** — so the leak was the normal case
and not the exceptional one. One of them held a pipe open and hung
`just standard` for two hours with its verdict already decided.

`leviculum_std::process::spawn_supervised` is the mechanism, and it takes
its `Command` **by value**, so that a supervised spawn and a bare one do
not look alike at a call site. Four things it has to get right, each of
which has bitten somebody:

1. **`PR_SET_PDEATHSIG` is per-task, not per-process.** The kernel stores
   it on the child's `task_struct` and delivers it from
   `forget_original_parent()`, which runs when the *forking task* exits —
   not when that task's process exits. A tokio worker or a
   `spawn_blocking` thread finishing mid-test would therefore kill the
   daemon under the test, which turns the fix into a flake generator. So
   **every supervised spawn is forked from one dedicated thread that
   never exits**, and the only event that ends that thread is the process
   ending. Nothing else in the design substitutes for this: the
   `getppid()` check below reports the parent's *thread group leader*, so
   a forking thread exiting while its process lives leaves it unchanged
   and the check sees nothing wrong.

   The same fact bites the measurement, not only the mechanism.
   `copy_process()` clears `pdeath_signal` for every new task, threads
   included, and libtest runs a test body on a spawned thread even under
   `--test-threads=1` — so a canary written as a `#[test]` reads its own
   parent-death signal as 0 while its process's main thread carries
   `SIGKILL`. That is why the probe below is a `fn main` and not a test.

2. **The race between `fork` and `prctl`.** If the parent dies inside
   that window the signal is already missed and the child runs on
   forever. After setting the flag the child re-reads `getppid()` and
   `_exit`s if it no longer names the process that spawned it.

3. **It does not reach grandchildren,** and the remedy is *not* `setsid`.
   A supervised child deliberately stays in its parent's process group,
   so `run-with-manifest.py`'s group kill still reaches everything below
   it — an orphan that has `setsid`-ed is the one thing that wrapper
   names as out of reach. Where a supervised process starts its own
   long-lived children, that is a separate link needing the same
   treatment at its own site.

4. **The signal is `SIGKILL`, and the reason is the state it fires in.**
   `PDEATHSIG` is delivered only once the parent is *already dead*, so
   there is nobody left to wait for a polite exit and nobody to escalate
   if the child declines. A catchable signal there is the same "cleanup
   that usually runs" the mechanism exists to replace, and the daemon
   whose graceful shutdown is being trusted is the one whose graceful
   shutdown hung a gate for two hours. The polite path is still tried
   first, by the owning `Drop`, and those destructors already end in
   `kill()` — so this is the same signal, moved to where it cannot be
   skipped. The cost is the child's own last wishes: `test_daemon.py`
   removes its `mkdtemp` config directory in a `finally:` block, and
   under `SIGKILL` that directory stays. Ports, sockets, ptys and
   `flock`s are released by the kernel on death, so the loss is a few KiB
   under `/tmp` in a run whose parent has already crashed.

**And the other half of the same rule, in the destructors.** A `Drop`
that owns an external process must reach the kill on **every** path.
`PyDaemon::drop` (`leviculum-ffi/tests/support/python_daemon.rs`) did
fallible I/O first — `query("shutdown")`, which `expect`ed a JSON
response and got the empty body a mid-shutdown daemon returns — and a
panic in a destructor that is itself running during unwinding is a
non-unwinding panic, so the process aborted before reaching
`child.kill()` two lines down. That is a second, independent reason the
same daemon leaked. The shape to write is: try the polite shutdown,
ignore every error it can produce, then kill unconditionally. It pairs
with `PDEATHSIG` rather than replacing it — the kernel covers "the parent
died", this covers "the parent lived and its cleanup threw".

**Which sites.** Every spawn whose process outlives the call that made
it: the Python `TestDaemon` and its `socat` pty pair, `PyDaemon`, the C
`lnsd`/`lncp`/`levcat` programs in the FFI suite, `lnsd` and the vendored
`rnsd` in the mvr, load-test, reverse-RPC and status-parity harnesses,
the instance-conflict holder — and, outside the tests, the
`PipeInterface` bridge program, which is the one long-lived process the
shipped daemon starts. Deliberately not covered: everything spawned and
awaited inside one call — `cc`, `git`, `stty`, `rnstatus`, the `jl` /
`jldiff` filters, the `event-log-helper` and port-allocator workers.
Those are counted rather than argued about; see the gate below.

## Standing canaries

Every gate on this page carries a permanent pair, checked before it
reports anything else:

- **A, registry gate**: a tagged test deliberately absent from
  `scripts/pins.txt` that must be reported, and a correctly registered
  one that must not be.
- **A, mutation audit**: a deliberately weak pin that must survive and a
  strong one that must die. Additionally: an unresolvable declared
  subject is a hard error, `mutants_generated > 0` is asserted per pin,
  and the equivalent-mutant allowlist carries per-entry justifications
  and is pinned like `scripts/ignored-counts.txt`.
- **B, manifest writer**: a fixture of libtest output, parsed before every
  wrapped run, holding tests that must land in the manifest — including a
  `#[should_panic]` one, which libtest names `<test> - should panic` and
  `--list` names plainly (a `no_run` doctest is the same shape, run as
  `<test> - compile`) — and lines that must not: an ignored test, an
  indented look-alike a test could print. The counts are then reconciled
  against libtest's own summary line, so a manifest that disagrees with
  the run fails the gate.
- **B, manifest check**: a test deliberately in no gate that must always
  be reported, and one in a gate that must never be.
- **B, gate termination**: a child that leaks a grandchild holding the
  gate's stdout and then exits — the wrapper must report the child's exit
  status promptly and must **name** what it killed — paired with a child
  that leaks nothing, which must report no kill at all, because a kill
  claimed on every green run is noise and noise is how a real one goes
  unread. Two further arms: a child that never exits, which must produce
  the named timeout rather than a wait, and an orphan that escapes the
  process group with its own `setsid()`, which must not delay the verdict
  past the drain grace. This canary is **bounded from outside the thing
  it tests** — a watchdog in the canary kills the grandchild by an argv
  marker after twenty seconds, so a regression fails in twenty seconds
  instead of wedging the suite the way the incident wedged the run.
  Walking into that trap while fixing it would be poor form. It costs
  about 0.85 s per wrapped gate, which is the price of the only check
  that can see a wrapper that has stopped terminating: every other gate
  on this page reports nothing at all when that happens, including the
  parser canary in the same script, which passes happily while the run it
  belongs to never ends.
- **B, supervised spawns**: two of them, because the property has two
  halves that fail independently. The **census**
  (`scripts/check-supervised-spawns.py`) classifies two fixtures before
  it reports anything about the tree — one holding a bare spawn in each
  of the four shapes the tree writes them in, all of which must be
  reported, and one holding a supervised call, a runtime `spawn`, a
  string and a comment that mention the words, none of which may be.
  Without the first arm a classifier that has stopped matching reports a
  clean tree forever; without the second it reports every line in it. The
  **behaviour** (`leviculum-std/tests/supervised_spawn.rs`) SIGKILLs a
  parent and requires its child to be gone, paired with the same
  experiment on a child spawned bare, which must still be alive after the
  same deadline — otherwise "the child is gone" is satisfied by a child
  that never started. Both arms are bounded and fail loudly rather than
  waiting, which is the mistake the incident above was about, and the
  child's own `PR_GET_PDEATHSIG` is checked first as the cheap form: a
  refactor that drops the `pre_exec` is named in milliseconds.
- **C, citation guard**: a deliberately drifted citation that must be
  reported. The existing guard has floor asserts against parser rot; the
  canary is the stronger form.
- **C, figure attribution**: a fixture page and a fixture doc comment
  attributing two figures to it — one on the page, one not — plus a
  version string that must not be read as a figure and an attribution to a
  page that is not in the tree. Exactly two must be reported. This one is
  not optional in the ordinary way: #200 was fixed hours before the check
  was written, so the corpus has no failing case left and a broken trigger
  would report zero forever against a tree that reads clean.
- **C, commit-trailer guard**: a message carrying the trailer that must be
  rejected and one that only quotes it, indented, that must not — both run
  before the guard reports anything else. leviculum's pinned count of
  sixteen below-baseline violations is the same canary in stronger form,
  covering both directions over 1275 real commits on every run; periculum's
  count is zero and covers only the direction that cannot rot, which is why
  the pair is in the script rather than only in the baseline file.
- **C, commit-trailer authorship arm**: the ours/foreign split needs its
  own pair, because "no violation found" is what a guard that has stopped
  noticing foreign trailers reports forever, and equally what one that has
  started calling *everything* foreign reports forever. Two independent
  failures, two checks. The **plumbing** is verified against git itself on
  HEAD — a `--format` that lost `%ae` would leave every commit classified
  as foreign and go green on our own violations from then on, and checking
  it against a constant in the script would only prove the script agrees
  with itself. The **classification** is verified on a synthetic set
  carrying one hit per declared identity plus one from outside: on a tree
  whose history happens to hold no foreign hit an inverted comparison
  would also read green, and a comparison tightened until it stopped
  recognising the forge's noreply address would be silent without the
  per-identity cases. What no canary reaches is an identity *deleted* from
  the baseline file, since that file is the only statement of who we are —
  that one is caught by reading the diff, which is why the list lives
  where a reviewer already looks.

A one-time demonstration at implementation time is not enough. A gate
that stops matching — a glob that no longer resolves, a parser that
returns nothing — is green forever, which is the defect this page exists
to remove.

## Composition

**A pin is exempt from no gate, and may not appear in the exception
list.** Nothing else here makes that true: a pin that is `#[ignore]`d
satisfies A — its negative control is present and correct — while no
gate observes its green.

## What may live in a git hook

A hook is the most tempting place to put a check and the worst place to
get it wrong, because it is the one gate that has an off switch every
author already knows. So the admission test is narrow:

> **A git hook may only contain checks that are fast, deterministic, and
> that fail for a reason the author can fix at that moment. Anything else
> belongs in a scheduled run or an explicit command.**

Three conditions, and a check has to pass all three. Two worked examples
from 2026-08-07, both of which were in hooks and neither of which should
have been:

- **The tier-2 staleness block** (`pre-push`) failed all three. It was
  slow by construction — the remedy it named was a 30-90 minute docker
  run. It was not deterministic in the sense that matters: its verdict
  depended on a ledger line written by a process nothing scheduled, so
  the same tree pushed on two days gave two answers for a reason
  unrelated to the tree. And the author could not clear it at all, at any
  moment, because `just extensive` — the remedy it printed — does not
  write the line it read. It blocked for 46 days and 502 commits. The
  full telling is under Guarantee B, above.
- **`post-commit`** failed the first and the third. It detached
  `scripts/run-tier1.sh` — `just standard` under docker, fifteen minutes
  warm and forty cold — after every commit. A commit cannot wait forty
  minutes, and a commit is not a unit anyone wanted tested in the first
  place: WIP commits, amends and commits mid-refactor each started a
  run, which is why the runner carried a dirty-flag loop to coalesce
  them — machinery repairing a granularity that was wrong to begin with.
  The third condition is the decisive one: when that gate came back red
  twenty minutes later, there was nothing the author could do about it
  *at the moment of committing*, which is the only moment a hook has. It
  was removed on 2026-08-07 and Tier 1 became an explicit `just
  standard`, once per batch.

The condition that keeps getting skipped is the third one, so state it
positively: a hook fires at a moment the author is still holding the
thing being judged. That is the whole value of the position — a red
`fast` names a file the author has open. A check whose result arrives
after that moment has passed, or whose remedy is somewhere other than the
work in hand, is not cheaper in a hook; it is only louder.

### The corollary, which is the part that bites

**A hook that is ever unsatisfiable trains people to bypass the whole
hook.** There is no partial override. `--no-verify` is one flag for all
of `pre-push`, so the 502 commits that walked past the tier-2 block also
walked past the pipeline lint, Tier 0, mvr and the commit-trailer guard —
checks that were working, that were fast, and that nobody had any
complaint about. The unsatisfiable check did not merely fail to protect
anything. It switched off the ones that did, and then went on being
green-adjacent in the ledger while it did so.

Two consequences worth writing down:

- **The cost of a bad hook is paid by the good ones.** A check's
  admission to a hook is therefore not a decision about that check
  alone, and "it can't hurt to also verify X here" is false as stated.
- **Bypassing becomes the habit, not the exception.** After the first
  few `--no-verify`s the flag stops being a considered override and
  becomes how one pushes. Removing the offending check does not undo
  that by itself; the habit outlives it, which is why the removal is
  worth recording where people read rather than only in the diff.

The same reasoning is why `.githooks/commit-msg` and the Tier 0 half of
`.githooks/pre-push` stay: milliseconds and ~3 minutes respectively,
deterministic given the tree, and each fails naming a file the author can
open. It is also why neither of them is the *enforcement* — a fresh clone
has no hooks at all. The forge check is the gate; the hook is the same
rule delivered early, at the moment it is cheapest to obey.

## What none of the three reaches

- **Whether the check is the *right* check.** A pin can be executed,
  carry a negative control, cite a live line, and still assert the wrong
  thing. That is what the reference-first and independent-recomposition
  rules in [Wire Field Semantics](wire-field-semantics.md) are for.
- **A negative control is author-chosen at both ends.** It proves the
  pin bites on one break the author thought of — not on the break that
  will happen. Mutation supplies an adversary who is not the author,
  which is the other reason the audit is not optional.
- **A control written to pass regardless** — verifying against a random
  key, asserting `is_err()` on something unrelated. It satisfies the
  registry gate; only the audit can see it.
- **Whether a test asserts anything at all.** A body of `let _ = f();`
  can be executed and coupled to its subject.
- **Prose.** Issue comments, commit messages, reports. One line shape in a
  commit message is now checked (#205, above); nothing a message *claims*
  is, and that is the surface that matters.
- **Scenario corpora.** A Periculum step that asserts nothing is the
  same defect in another language; its analogue is the delivery bar
  (Periculum #25).
- **Firmware.** `leviculum-nrf` is excluded from the workspace
  (`Cargo.toml:39`) and cross-compiles. All three stop there, and the
  sx1262 incident lives on the far side.

## Where this stands

Codeberg is the source of truth for what is built. At the time of
writing: C covers `docs/src/**` and the Rust sources of `leviculum-core`,
`leviculum-lxmf` and `leviculum-std`, and its submodule check runs first
in `just fast`; its bump path is unbuilt. Prose attributions in Rust doc
comments are checked for decimal figures and for nothing else. Its
commit-trailer step runs on every push to either repository, and checks
one line shape and no claim. B emits manifests from every
host gate that runs tests, and `just complete` in the `extensive` tier
runs the whole workspace by construction, so no ordinary test is outside
the union; the check that reads that union, and the staleness bound that
ages manifests out, are unbuilt. Its wrapper terminates on its child
rather than on the pipe, kills the child's process group and reports what
it killed, and gives up at 1800 s with a named failure. The spawn-site
rule above is built and audited: every long-lived spawn goes through
`spawn_supervised`, the eleven bare spawns that remain are pinned per
file in `scripts/supervised-spawn-counts.txt` with a reason each, and
both halves run in `just fast`. What it does not reach is a process a
supervised child starts for itself — a separate link, covered only by the
wrapper's group kill — and any platform that is not Linux, where the
helper compiles to the `Drop` path and says so. A is unbuilt.

All three are subject to the rule they enforce. The standing canaries
above are the demonstration made permanent, because a one-time one
decays.

## See also

- [Evidence and Honesty](evidence-and-honesty.md) — the rule this page
  mechanises, and the incidents behind it.
- [Wire Field Semantics](wire-field-semantics.md) — what a pin must
  assert, which is a different question from whether it can fail; and
  the practice of checking the reference before auditing against it,
  which Guarantee C mechanises.
