# The test-quality audit campaign — status

**Status:** Working status record, never normative. **This is a status document and is expected to
be edited in place** as tracks land; it is not a dated memo. The plan it executes is held outside
the repository; what is durable is here and in the dated memos each track produces.

⊘ **This tracker has no pointer in `CLAUDE.md`.** It follows the convention
[`artifact-delivery.md`](artifact-delivery.md), [`client-delivery.md`](client-delivery.md) and
[`ingest-campaign.md`](ingest-campaign.md) use, the first two of which are named there by owner
direction. Whether the campaign is tracked here or on issues is the owner's to settle.

**Last updated:** 2026-08-30.

---

## 1. What the campaign is asking

Not *does this test assert?* but **can this test fail, and does it fail for the reason its name
claims?** — and, separately, **what claim has no test at all?**

The distinction matters because the obvious answer was already checked and came back clean. A
reconnaissance pass across twelve crates found **zero** `assert!(true)`, zero self-comparing
assertions, zero commented-out tests and no tautological round-trips; assert density runs three to
four per test and roughly 90% of sampled bodies are load-bearing. A campaign hunting lazy
assertions would find nothing and would report that as health.

What it is hunting instead, in the four forms Wave 0 and the reconnaissance established:

- **Unreachable** — the test is green because nothing ran it.
- **Under-discriminating** — it fails, but not for the reason its name gives. The repository holds
  95 bare `assert!(x.is_err())`, and on a fail-closed path an error of the wrong kind passes.
- **Mis-named** — the body checks less than the name claims. The name is what a reviewer greps
  for, so the name is what misleads.
- **Absent** — a normative claim with nothing that would go red if it were violated.

Each track applies those as four lenses (L1 reachability, L2 discrimination, L3 fidelity,
L4 absence) and reports into its own dated memo.

**Method.** Read the test against the design section it claims to cover; that is the default and
usually settles it. Mutate — break the code in a throwaway worktree and check the test goes red —
only where reading leaves genuine doubt. Owner ruling, 2026-08-30: certainty from reading needs no
mutation.

**Auditors are read-only.** Findings and fixes are separate waves, because an agent that
implements stops being able to review its own work honestly
([`agents/parallel-work.md`](agents/parallel-work.md)).

**A clean subsystem is a valid result**, and every brief says so. An audit that returns twenty
findings from a healthy surface has destroyed its own signal.

## 2. Severity

On `CLAUDE.md`'s own three questions, not on a generic scale.

| | Meaning |
|---|---|
| **S1** | Bears on a disclosure surface — an invariant or Appendix C row whose test cannot fail, or a claim with no test at all |
| **S2** | Bears on something irreversible — entity ids, term ids, tombstones, WAL durability |
| **S3** | A real defect could pass unnoticed |
| **S4** | Weak but not misleading |

## 3. Where the campaign is

| Wave | Track | Surface | State |
|---|---|---|---|
| 0 | Reachability | the executed-vs-existing diff, and CI | **Landed** — see §4 |
| 1 | R1 | engine / query path | **Landed** — 3 findings (1×S1, 1×S3, 1×S4); **all fixed** |
| 1 | R2 | engine / artifacts | **Landed** — 4 findings (3×S1, 1×S3); **all fixed** |
| 1 | R3 | engine / write path | **Landed** — 3 findings (2×S3, 1×S4); Rule S/F **covered, proven by mutation**; F1–F2 fixed, F3 is an owner question |
| 1 | R4 | store + roaring | **Landed** — 3 findings (1×S1, 2×S3); **all fixed** |
| 1 | R5 | authz + filter + filter-write | **Landed** — 3 findings (1×S1, 1×S3, 1×S4); **all fixed** |
| 1 | R6 | lifecycle + types + wire | **Landed** — 5 findings (1×S1, 1×S2, 2×S3, 1×S4); four fixed, the S2 is owner item 5 |
| 1 | R7 | build + corpus + analyse + spatial | **Landed** — 5 findings (1×S2, 2×S3, 2×S4) |
| 1 | R8 | server + cli | **Landed** — 4 findings (1×S1, 2×S3, 1×S4) |
| 2 | P1 | `conformance/tests` + `conformance/suite` | Not started |
| 2 | P2 | `reference/oracle` + `reference/tests` | **Landed** — 6 findings (4×S3, 2×S4); **the oracle is independent** |
| 3 | T1 | `clients/ts/core` | Dispatched |
| 3 | T2 | the remaining TypeScript workspaces | Not started |

**P2 is the highest-leverage track and is not optional.** Every differential result
`conformance.md` reports rests on the oracle being independently right; a bug there that mirrors a
bug in the engine makes the differential agree. Its own tests are deliberately outside CI.

## 4. Wave 0 — what landed

Full account in
[`evidence/memos/2026-08-30-test-reachability.md`](evidence/memos/2026-08-30-test-reachability.md).

**Measured.** The workspace harnesses list **2,296 entries** (2,283 distinct names; thirteen names
are carried by two crates). **22 are `#[ignore]`d**, so the per-PR gate executes **2,261** distinct
names, or 2,274 counting entries. Both denominators appear in tooling output and both are correct;
the gap is the duplicate names.

**The resolver-2 coupling, confirmed and then fixed.** Two tests in `tests/viewport.rs` were
`#[cfg(feature = "bench-timing")]` and compiled under `cargo test --workspace` *only* because
`tessera-bench` declares that feature on by default and cargo unified it onto the normal edge —
they vanished under `cargo test -p tessera-engine`. One of them,
`rows_in_ranges_is_mask_independent`, is the only test of the mask independence Appendix C's C4
numerator rests on. Four more tests kept their names under `-p` while silently losing an assertion
arm to the same mechanism.

Nothing was untested and CI never missed them — but which leak-register rows have a test was being
decided by a benchmark crate's default feature. Both `tessera-engine` and `tessera-server` now name
`bench-timing` on their self dev-dependencies, so the two selections agree. `-p tessera-engine`
went 803 → 805.

**No row of `conformance.md` §4.6 rests on an unreachable test.** The uncovered rows (I5, I6, I8,
I11, I13b) have no test hiding behind a feature gate.

**Three CI defects fixed.** `cargo test --workspace` had no `--no-fail-fast`, which `CLAUDE.md`
records as already having been mistaken for a green gate here. Neither `scripts/check-clients.sh`
nor `clients/py/check.sh` ran in CI at all, so the TypeScript client's 524 tests and the Python
package's 21 were gated only by whoever remembered them locally; a `clients` job now runs both.
And the header's timing figures, which the workflow presented as a standing budget, are reframed
as measurements with a date — kept rather than withdrawn, because four other sites cite the 94 s
and withdrawing it in one place would have orphaned all four.

**Every `#[ignore]` now carries a reason.** Ten were bare, all measurement harnesses rather than
silenced assertions, but the attribute said nothing and establishing that took ten module-doc
reads. `scripts/check-test-reachability.sh --quick` is in the gate and in CI's `rust` job by owner
ruling, and refuses a bare `#[ignore]`. It is a developer-facing lint, satisfiable in the edit that
trips it — the class `clippy -D warnings` already occupies — and not an instance of the
operator-facing refusal `CLAUDE.md` warns against.

## 5. Wave 1 — the three engine tracks

761 tests assessed across the engine: 250 on the query path (R1), 305 on the artifact side (R2),
206 on the write path (R3). **Ten findings**, four of them S1. Each track's memo carries the full
account and a section of attacks that failed, kept so they are not re-run.

**The engine's tests are strong, and the findings are about their edges rather than their centre.**
Three conventions do real work and are worth naming because a rewrite would lose them: per-test
*"mutations this kills"* paragraphs; non-vacuity guards (*"or this test proves nothing"*);
and a wide *and* a narrow credential on every masking claim, with the narrow one asserted exactly.
No test on any of the three surfaces was found that cannot fail at all, and every zero-assertion
body delegates to a helper that asserts.

**The one result that most needed proving, and was proven.** Rule S / Rule F — the deny-retirement
conflation that is fail-open and has been caught in review twice — **is covered and it
discriminates**. R3 mutated `FoldPlan`'s `D₀` from `overlay.deleted_entities()` to
`overlay.denied()`, exactly the union §5.4 forbids, and `fold.rs:1113` went red for the right
reason while 27 of the binary's 28 stayed green. The catch is specific, not incidental. The test
even documents the mutation it is built to kill.

**The four S1s, and a caution about reading them as one thing.** Three of the four are a *missing*
test and one is a test that cannot fail; it is tempting to compress them into a single shape —
"a claim asserted only under a fixture that cannot distinguish it holding from it being vacuous" —
because that shape describes the most vivid of them. **Resist that.** Across all ten findings the
classes were five *missing*, four *under-discriminating* (in four different forms), one *mis-named*,
and none vacuous or unreachable. The credential case below is one finding of ten, and it came from
the one surface where credentials are the fixture axis.

The lesson that does generalise is duller and more useful: **absence was the largest class, and L4
is the lens most easily skimped**, because it means enumerating the design's claims and checking
each has a test rather than reading tests and judging them. Alongside it, one fixture-agnostic
question — *would this test still pass if the thing it names were replaced by something trivial?*

Recorded because the controller briefed the second half of Wave 1 on the compressed reading first
and had to correct it mid-flight: a vivid finding is not the same as a frequent one, and a brief
that names a shape will get that shape back.

- **R2 F1** — the fold's generating-set shrink is asserted only from a full-coverage credential,
  and a full-coverage mask contains every generating set including an empty one. Replacing
  `andnot_inplace(retired)` with `clear()` in `membership.rs:63` left the test green, the binary
  green and all of `-p tessera-lifecycle` green. A fold that *erased* `G` rather than shrinking it
  would serve corpus-derived text on a vacuously-contained empty set and nothing would say so.
  The fix is one credential.
- **R2 F2** — no test anywhere applies a filter to an artifact carrying a non-empty generating set,
  so I12's third conjunct (containment identical with and without a filter) is never evaluated
  under one. Containment is `NothingToContain` in every filtered request in the repository.
- **R2 F3** — I8's headline arm, *later arrivals are not added to the generating set*, has no test:
  `artifact_growth.rs` publishes no content at all, so none of its fifteen cases could observe it.
  `conformance.md` §4.6's note that the fold tests cover both arms in substance is true of the two
  *deletion* arms only.
- **R1 F1** — no test varies `segments_version` across a region-leaf answer, so the region cache's
  I11 obligation, stated at its own key, is not discharged. The superseded generation's
  decompositions are deliberately kept resident, so the key field alone prevents a stale hit.

**Controller's verification.** Every finding above was checked against the source before being
accepted. Two evidence sentences did not survive that check and were corrected in place: R1's F1
originally said `region_leaf.rs` never publishes — it does publish an *artifact*, which advances a
level version and not `segments_version` — and R2's F2 originally said all three filtered files
declare `supplied: Vec::new()`, which is false of `artifact_filter_bit.rs:374` and confuses a
layer's content schema with a published artifact's `generated_from`. Both findings survived; both
sentences were wrong in a way a reader attacking the memo would have found first.

## 6. The fix wave — nine findings closed

Two implementer tracks, **neither of them the track that found the finding**. The reason is on
record because it is the campaign's own logic turned on itself: a fix here means writing a test that
*bites*, and the characteristic failure of someone fixing their own finding is writing one that
merely passes. The memos carried the context across the agent boundary, which is what they are for.

**Every fix carries a four-state receipt** — mutation applied → old test still green (the receipt
that the gap was real) → new test red → mutation reverted → green. A fix that could not produce
state 3 would not have been a fix.

| Finding | Fix | Receipt |
|---|---|---|
| R2-F1 | `artifacts_of` delegates to `artifacts_for(engine, credential)`; the post-fold service is asserted from a narrow principal, with a second artifact `c1` as a positive control so "served nothing" cannot pass as "correctly withheld" | `andnot_inplace(retired)` → `clear()`: old 27/27 green, new red at `left: [c0, c1], right: [c1]` |
| R2-F2 | a filtered/unfiltered/matches-nothing comparison over generating sets drawn **across** the filtered value | defect on the containment mask: four pre-existing filter tests green, new one red |
| R2-F3 | a growth whose joiners the narrow principal cannot see, asserting `G` unchanged while membership grew | joiners `or_inplace`d into `generated_from`: old 12/12 green, new red `left: 0, right: 1` |
| R2-F4 | four distinct message substrings, plus a check that the four are four | two rules collapsed onto one refusal path: old green, new red quoting the wrong refusal |
| R1-F1 | a region answer re-taken across a **merge** that renumbers rows — a merge keeps `prefix` and moves only `segments_version`, so the term under test is the only one that changes | `segments_version` held at `u64::MAX`: four pre-existing cases green, new red `left: 15 right: 16` |
| R1-F2 | a `#[cfg(test)]` module in `src/region.rs`, which had none | `is_of` → a length comparison: 279 lib tests green, new one red |
| R1-F3 | renamed to `item_lookup_resolves_a_row_far_from_the_segments_start`; doc now says plainly that a column scan would pass it | none — a rename needs none, and the fixer said so rather than inventing one |
| R3-F1 | the floor driven deterministically by parking the executor at `PauseSite::AfterFsync`, gauge read behind a barrier | **pinned from both sides**: constant → 6,400 gives `left: 2 right: 3`; constant → 32 gives `left: 2 right: 1` |
| R3-F2 | the publication wait extracted into `wait_for_fold_publication`, which carries its own deadline | the four-state form does not apply — see below |

**One receipt was reframed rather than faked, correctly.** R3-F2 is about a failure *report*, not a
failure: the tests caught their defects before and after. So instead of a mutation, the fixer
injected a 1.9 s delay into the hold and showed the **old** shape goes red at `fold.rs:1306` naming
the *wrong* step, while the new shape stays green; at 3 s the new shape goes red naming the right
one. That is the right demonstration for that finding, and it was labelled as a substitution rather
than passed off as the standard form.

**Two things the fixers found that the auditors had not.** Track B's region test must take both asks
at the **same segment count**, or `rows_under`'s `debug_assert_eq!` trips first and a debug build
reports a panic instead of the wrong answer the test hunts — documented at the test. And
`OVERLAY_PUBLICATION_MAX_WINDOWS` is private to `write.rs`, so the test restates 64 with the
coupling marked deliberate; the fixer declined to make the constant `pub` because that was outside
its allowlist, which is the correct stop.

**Gate after the fix wave:** `cargo test --workspace --no-fail-fast` exit 0; clippy clean;
`check-layers`, `check-test-reachability --quick` and `check-doc-links` green. **2,301 entries /
2,288 distinct names, 22 ignored, 2,266 executed** — five new tests, reconciling exactly with the
five cases added.

## 6b. The second fix wave — ten findings closed

Two implementer tracks again, neither of them the track that found the finding, and every fix
carrying the four-state receipt. Highlights rather than a full table; each memo has the detail.

**The one that could have gone wrong went right.** `tessera-roaring` exists for a cost property —
*bitmap operations cost O(containers touched), not O(cardinality)* — and tested only correctness: a
`push_block` rewritten to insert values keeps all seven tests green while the projection goes
~1.3 s → ~8.3 s at 10⁹. The obvious fix is a timing assertion, and `correctness-suite.md` §17
forbids one: *"a timing assertion in a test that runs on developer machines is a flake generator"*.
The brief therefore required a **structural** observable or an explicit stop-and-report. One exists:

```rust
assert!(sink.out.is_empty(),
    "no entity may reach the result before a stream is handed to croaring: \
     value-by-value insertion is what this crate exists to avoid");
```

A per-value sink populates `out` immediately; a block-staging sink leaves it empty until `flush`.
The cost property is now pinned by a structural fact, with no clock in the test.

**The others.** `coalesce_dict_extents` has the test its own doc called "the whole of its
correctness argument" — five extents, the middle three coalesced, every descriptor asserted to keep
the ordinal it had. The self-comparing fragment test now builds in **two separate caches** with a
guard saying why (*"or the second is a Ready hit on the first and this test compares a value with
itself"*), and covers the composition — a `coalesce_delta_tiers` output read by
`build_fragment_with_deltas` — that nothing exercised. The fold's `permutation.bin` and
`row-entity.u32` are asserted to invert each other across a tombstoned fold. Four length-consistent
corruptions isolate the two width rules on both row columns, asserting on `detail` because every
framing refusal there returns the same variant. The nested-list refusal asserts the depth rule fires
**and** that a bounds error does not, so the guard cannot be bypassed by a truncation behind it.
The empty I10 test is **deleted**, its prose kept and rewritten to say what is true. `tessera-types`
goes **17 → 39** under `-p`, with 0 workspace-only tests. The frame header has a byte-level test
with a guard that the fixture's length distinguishes the two byte orders.

**Gate after both fix waves:** `cargo test --workspace --no-fail-fast` exit 0 — **2,283 passed, 0
failed, 22 ignored**; clippy clean; `check-layers`, `check-test-reachability --quick` and
`check-doc-links` green. **2,305 entries / 2,292 distinct / 2,270 executed.**

**A small inconsistency between tracks, recorded rather than churned.** `scripts/fmt-file.sh`
reformats pre-existing non-test code in any file it touches, and this tree is not `cargo fmt`-clean.
One track kept those reformats (the repo convention is that a change formats the files it touches);
another reverted them to keep its diff test-only and named the three spots it left un-normalised —
two statements in `crates/tessera-store/src/membership.rs` and a blank line in
`crates/tessera-roaring/src/lib.rs`.

## 6c. Wave 1 complete — the Rust estate

R7 and R8 close it. **~2,213 tests assessed across every Rust crate; 28 findings.** By class:
**twelve missing**, nine under-discriminating, two vacuous, three mis-named, one unreachable, one
S2. Absence stayed the largest class on every surface that had one.

**Two hypotheses this campaign held were wrong, and the negative results are worth more than the
findings they displaced.**

*The server harness was called the highest-leverage hour on its surface* — 1,050 lines that all 314
server tests run through, where a quiet normalisation would weaken everything above it invisibly.
It normalises **nothing**: zero sorts, zero `unwrap_or_default`, zero retries, and 34 assertions it
*adds* to every test above it. `decode_viewport_frames` enforces `contracts §3.2 r26`'s frame
grammar centrally, closes the trailer's key set, and reads columns **positionally on purpose** so an
inserted column fails loudly instead of silently rebinding. The 314 tests are stronger than they
look, not weaker.

*`tessera-build` was expected to dominate the findings* as the largest single body on R7's surface.
Its 367 tests yielded **one** finding; four of five were in the two smallest crates. Size did not
predict weakness anywhere in this campaign.

**The sharpest finding of the wave is a test whose own message describes a guarantee it does not
give.** `crates/tessera-analyse/tests/golden.rs:65` compares the recorded identity against
`Analyser::identity()`, which is `format!("{UNICODE}/{UNICODE_VERSION}")` over two hand-maintained
constants. Its failure message names two cases and calls the second "the one that silently
invalidates every index already built" — the vectors edited without moving the version. In that
case both sides of the comparison are the same unchanged string, and nothing in the repository would
notice. Analyser identity is recorded in manifests and a text column is read back with the analyser
it was written with, which is why this is **S2** rather than S3.

**The other one that bites:** the viewer plane has no test that `/v1/items/{id}` or
`/v1/artifacts/{id}` refuses a missing credential, and nothing enumerates it. Three of five routes
have a 401 test; the viewer router mounts no credential layer, so unlike the control plane there is
neither structural cover nor an enumerating test — and the control plane has exactly that test,
carrying a comment that it exists because `/control/status` once shipped unauthenticated. Every
current handler does follow `bearer_token` with `authenticated_session`, so the exposure is a new
route written without either, not a one-line deletion.

**Two things closed rather than found.** Wave 0's byte-identity pair now genuinely exercises the
parallel branch under `-p` as well as `--workspace`, so its names are honest and the finding is
closed. And the 2026-08-14 red-team's third latent hazard — `min_visible_members` accepted and
inert — is **superseded, not outstanding**: the key was deleted under decision 0085.

**A brief error worth recording.** R8's brief listed eleven server test files; there are fourteen.
`authored_shape_space`, `meta_projection` and `projected_ingest` were missing from the inventory the
controller inherited. R8 audited them anyway rather than working to the brief, which is the
behaviour wanted — but a scoped brief is a claim about the surface, and this one was wrong.

## 6d. P2 — the answer the campaign was built to get

**The oracle is independent where independence is load-bearing.** That is the claim
`conformance.md`'s authority rests on, and it holds. Every construction that would make a
differential circular is absent: geometry comes from the points file the build consumed, and
`Bundle._require_source` **refuses** rather than falling back to `morton.u32`; the filter and text
oracles read the fixture's generation functions, never `attrs/`; θ's anchor is computed, never read
back; the mask comes from flat `pairs.parquet` where the engine uses compressed postings; and
`theta_cut` is written as §7.2's recurrence where the engine evaluates the closed form, deliberately.
The two echoes that exist are **declared at the site** — `viewport.Selection` sorting on the stored
`tessera_id` column, and `text.py` reaching `tessera tokenise` (decision 0070) — and one of them
explicitly withdraws an earlier draft's stronger claim.

§0's *"every differential and every sweep here now has something that makes it fail"* **stands as
written**, and its own narrowing is honest. Live controls confirmed for I7 (the strongest), I1's two
defective engines, I2, θ's `saw_partial` guard, I3's `l-core` control and I10's per-mechanism plants.
The pass-only set is now enumerated rather than gestured at.

**The sharpest finding is that the check enforcing independence does not cover the module most
likely to break it.** `reference/tests/test_oracle_layering.py:20`–`:22` is a literal twelve-name
tuple; `reference/oracle/` holds fourteen modules. `text.py` and `label_fixture.py` are in none of
the three lists — and `text.py` is both one of the four definitional value oracles and the one
carrying a declared echo. It is the allowlist failure `catalogue.py`'s own doc argues against.

**Two more.** The four value oracles — `viewport`, `mask`, `filters`, `text`, ~1,150 lines carrying
the substance of I1, I7 and I12 — have **no test of their own anywhere**; each is checked only
against the implementation it exists to check. And `reference/tests` is excluded from CI *wholesale*
when only two of its six modules need the absent corpus: **62 of 71 tests run green in 21 s without
it**, and the excluded set holds the oracle's only known-answer test of the identity permutation,
while the Rust half of that pair does run. `ci.yml`'s comment has been corrected — it said "two of
its five modules", and it was written in Wave 0 of this campaign.

**One finding corrects an earlier track.** R8 reported the 2026-08-14 red-team's `min_visible_members`
hazard as *superseded*, the key having been deleted under decision 0085. P2 found
`reference/oracle/harness.py:722` **still writes it into every server config**, and `[disclosure]` is
the one section parsed as a raw table rather than under `deny_unknown_fields`, so it is accepted and
inert. Nothing depends on it today, so nothing passes vacuously; a test written tomorrow would. The
hazard is narrower than the red-team recorded and wider than R8 concluded.

## 6e. The oracle fix wave — P2's six closed, and 69 known-answer tests added

**The layering check now fixes the class rather than the instance.** `test_oracle_layering.py` globs
`reference/oracle/` and asserts both directions — no module outside the three groups, no classified
name matching no file — so a new module fails until someone places it. Its receipt is the sharp
part: a probe module importing `harness` makes the new check fail by name while the **old
hard-coded list passes**, because the probe was not on it. A merely longer list would have
reproduced the defect one file later.

**CI now runs the corpus-free oracle tests**, and the choice of mechanism matters. Told it could
list the four wanted modules positively or name the exclusions, the track chose exclusions on the
grounds that *a list of what to run is a list a new file is silently absent from — the same defect
the layering check had one level up*. Concretely: the 69 new value-oracle tests landing in the same
hour would have fallen outside a positive list, and instead ran the day they arrived. The step
carries `working-directory: reference` because `--deselect` node ids resolve against pytest's
rootdir — a repo-relative deselect **matches nothing and is not an error**. The controller
reproduced that silent-exclusion trap independently while verifying: from the repo root, 3 errors;
from `reference/`, 132 passed and 3 deselected.

**The four value oracles now have a third statement.** 69 known-answer tests across `viewport`,
`filters`, `mask` and `text` — corpus-free, 0.15 s — each docstring naming the rule, where it is
stated, and the arithmetic where a number is derived. **40 mutation receipts**, all red; two
survived a first pass and the tests were strengthened until they did not, which is recorded rather
than smoothed over. θ is pinned on all six of its behaviours, including saturation as a *state*
rather than a clamp and the floored quotient worked from 2⁶⁴/3 where floor and ceiling differ.
Masking-before-selection (I7) is pinned by a row whose entity sits outside the mask, asserted absent
and then admitted by widening the mask alone.

**The honest limit, in the track's own words.** For `viewport`, `mask` and `filters` the
mirrored-defect risk is *materially reduced rather than merely narrowed* — every rule reducible to a
number now fails against arithmetic written out in the docstring rather than against the other
implementation. What remains: **the fixtures are one author's, so a rule misread is a rule pinned
wrongly, and the third opinion is a reading of the design rather than an independent derivation.**
Mitigated by quoting §7.2 and `contracts §3.2` verbatim into the working so a reviewer can check the
arithmetic without re-deriving it. `text.py`'s segmentation remains a single-implementation echo by
design (decision 0070), and the new file says so at the top rather than leaving it implicit.

**An oracle/engine divergence found by the fixer, correctly left unpinned.**
`TextColumn.matches(entity, ["quick"], 0)` answers `True` for any entity carrying a token stream
(`need = 0`), where the engine answers the empty set. It is unreachable from the wire —
`crates/tessera-server/src/filter_dto.rs:420` refuses `minimum_should_match` below 1 with a 422, so
no request can produce the input. Pinning the oracle's answer would ratify a divergence; pinning the
engine's needs an edit to `text.py`. The docstring records the input as unreachable rather than
asserting on it. **S4 at most, and it wants a disposition rather than a fix.**

## 7. Open for the owner

Six things Wave 1 surfaced that an implementer must not settle. Each is stated so it can be ruled
on without opening a source file.

**1. `conformance.md` §4.6's I8 note is wrong, and the row may need to move.** The matrix records
that "the engine's own fold tests cover both arms in substance". R2 found that true of the two
*deletion* arms only: I8's headline arm — a generating set is immutable, so **later arrivals are
not added to it** — has no test anywhere, because `artifact_growth.rs` publishes no content at all.
Fix track A is writing that test now. The question is whether §4.6's wording is corrected when it
lands, or whether the row's position changes with it. Decision 9 governs: a row moves only when a
test moves with it — and one is about to.

**2. §4.6's I11 row could be narrowed.** It reads as a flat regression in coverage. R2 reports the
cross-request half has in fact been rebuilt on the artifact side — every row-space artifact carries
a validity coordinate with a both-directions adoption test, and one case drives a real merge that
permutes row space inside a prefix. What is genuinely uncovered is the *within-request* clause, plus
the region cache (R1's F1, also being fixed now). Narrowing the row would make the register say
something truer; leaving it says something safer. R2 did not move it, correctly.

**3. RULED, 2026-08-30 — a generating set with no survivors is not served.** *(Was: a design
silence at C7's limit case.)* The owner's ruling settles what
`annotation-write-cycle.md` §2.1 and Appendix C's C7 did not say. Being implemented: a decision
file, both design sections, the fold, and a test.

The ruling closes a loop the code already half-states. `crates/tessera-lifecycle/src/registry.rs:654`
**already refuses** an empty generating set at publish for content requiring all members visible,
and its refusal message gives the reason — *"an empty set is satisfied by everyone"*. `:647` refuses
the converse, so inherited content's legitimately-empty set (C28) is a different case and is not
touched. **The only route to an empty `All` generating set is the fold's permissive shrink**, which
is exactly what was ruled on: the design already knew the hazard, and the fold was not held to it.

Mechanism: the fold **withdraws** the content when a shrink empties it, landing the artifact in the
existing `layer_declares_content → Unsatisfied` branch — the same state a strict withdrawal already
produces. No new `Containment` arm, and decision 0076's prohibition on the in-between state stays
intact.

**3b. The original silence, for the record.** `annotation-write-cycle.md` §2.1 and Appendix C's C7
both describe a permissive shrink as serving "a principal satisfying the survivors", and neither
states a position on there being **no** survivors. R2 probed it: an emptied generating set does not
reach a zero-credential principal, because an artifact no visible member of which is in view is not
a candidate — so the widened channel reaches only principals who see some member, C7 at its widest.
**The code is correct and this is not a defect.** But the design does not speak to its own limit
case, and the test that will now pin it (fix track A's F1) is asserting behaviour the corpus does
not describe. Either the design says what happens when `G` empties, or the test records that the
question is open.

**4. Whether the merge/deny race can now be made deterministic** (R3's F3). `merge.rs:494` catches a
lost deny on only one of two orderings, by its own documented account, and the reasoning that a
deterministic form was out of reach **predates `merge.rs:1026`**, which builds a pause site. Whether
that site can carry the case is a question about what the pause machinery is for, not a defect. R3
recorded it rather than acting.

**5. Durability ordering needs a seam, not a test** (R6's F2, **S2** — the campaign's only S2).
An ack-before-fsync WAL passes all 122 `tessera-lifecycle` tests. R6 proved it three ways, each
122/122 green: `sync_data()` reduced to `Ok(())`; every fsync in `wal.rs` removed; and the sharp
one, the sidecar offset **published before** `sync_data` runs — a genuine defect at the one function
whose whole contract is that ordering.

**No existing test can be adapted.** The four "real fsync failure" tests all provoke the
*sidecar-publish* half; the source itself says the `sync_data` half cannot be provoked in-tree; and
the fault switchboard sits at the `ExecutorWal` wrapper, returning before `Wal::fsync` is entered,
so the real `sync_and_publish` never runs under injection. What would make it falsifiable is a
fault hook **inside** `Wal::sync_and_publish` at the `sync_data` call, so a test can read the
sidecar off disk after a sync-half failure. That is a production change and a decision about what
the fault machinery is for. `conformance.md` already records this as a non-row and the roadmap
carries it as [#71]; what is new is that the gap is now precisely located.

**7. A ⊘ marker that records built machinery as absent** (R7 stopped on this rather than editing
it, correctly). `correctness-suite.md` §11's marker says "Every bullet above is new", that the built
verifier's identity loop "restarts its row index" per segment, and that this is "wrong for any
bundle that has flushed". Every bullet is implemented in `crates/tessera-build/src/deep.rs` and
damage-tested in `crates/tessera-build/tests/verify_deep.rs`, and
`crates/tessera-build/src/lib.rs:1660`–`:1673` looks the row base up from the row space per segment
with a comment saying why — a multi-segment flushed bundle verifies in test today.

**This one differs from items 1–4 in kind.** Those are judgements about whether a coverage row
should move. This is a plain factual error, in the direction `CLAUDE.md` singles out: *a register
that records built machinery as absent understates its own gap*, and *we cannot test this* and *we
have not tested this* are different claims. `design-process.md` exempts this class from the full
process — stale cross-references and figures the evidence contradicts — so it is a one-edit fix,
plus regenerating `inventory.md`'s ⊘ count. It is here rather than done only because it is a
coverage claim, and every other coverage claim in this campaign has been left to the owner.

**6. The padded-manifest refusal is specified and not built, and the audit has now traced its exact
path.** `contracts §2.1` requires a reader to **refuse** `SEGMENTS-007.json` rather than parse it,
and carries its own ⊘ marker saying the refusal is not implemented. R4 confirmed the marker is still
accurate and traced the consequence: `read.rs:916` parses any decimal, `read.rs:793` reconstructs
the canonical unpadded name, the open fails, and `read.rs:795`–`:799` steps past — carrying the
reader past a manifest that may hold a `deny` list. Nothing in-repo writes padded names
(`manifest_write.rs:229`), and pre-release there are no deployments, so nothing is exposed today.

It is recorded here because implementing the refusal is a **code** change and this campaign's fix
waves are about tests; and because the ⊘ note's protection — "nothing writes padded names today" —
is a statement about the current writer rather than about the reader, which is what §2.1 says must
hold. There is also no test for the refusal, but that is a consequence of it not existing, not a
separate finding.

## 8. Cross-cutting findings

Recorded here when a finding belongs to no single track, or when several tracks meet the same
thing.

- **The resolver-2 coupling is systemic, not the single instance Wave 0 reported.** Wave 0 found
  two `bench-timing`-gated tests in `tessera-engine` and fixed them, but ran its per-member sweep
  for that crate **only** — a scope the memo did not state, so it read as a settled workspace-wide
  result. Track R6 found the same mechanism in `tessera-types` at a far worse ratio, and the full
  sweep, run afterwards, confirms **22 tests reachable only under `--workspace`**, all of them
  `layer.rs`'s `#[cfg(feature = "serde")]` cases, unified on only through `tessera-lifecycle`'s
  dependency. `cargo test -p tessera-types` runs **17 of 39**.

  Every one of them still runs in CI, which uses `--workspace`; what is wrong is that a developer
  working in that crate sees under half its tests, and which tests exist is decided by an unrelated
  member's feature. The remedy is the one Wave 0 already applied twice: name the feature on a self
  dev-dependency. Wave 0's memo now carries a correction saying what it actually swept.

- **The sweep's first run reported 23, and the twenty-third was a defect in the sweep.**
  `an_undersized_bound_does_not_livelock` exists in `tessera-authz/src`, `tessera-engine/src` and
  `tessera-engine/tests/cache.rs`; `--list` names are not crate-qualified, so the bare spelling from
  the third was attributed to `tessera-authz`, whose own listing carries it under
  `single_flight::tests::`. `scripts/check-test-reachability.sh` now drops a candidate whose final
  segment the crate's own listing already contains, and the comment records the trade. Worth
  keeping in the record rather than quietly fixing: **the tool built to audit the tests had the
  same class of defect the audit is looking for** — a check that reports something true-looking
  which nobody had made fail. It was caught by verifying its output against the source, which is
  the campaign's own method turned on its own instrument.

- **Containment's mask arm has no test in the suite, and is the arm every non-builtin plugin
  takes.** Surfaced by fix track A while building F2's receipt, not by an audit track.
  `ArtifactRows::containment` (`crates/tessera-engine/src/artifacts.rs:2162`) answers "by the
  partition where there is one and by the mask where there is not", and the field's own doc at
  `:170` says the partition is **`None` under any plugin but the builtin**. Every engine fixture
  builds a level with a partition, so instrumentation during the fix showed the fallback at `:2173`
  is never reached in any engine test — which is why a defect planted there left the whole binary
  green and the fix had to plant it on the arm requests actually take.

  The arm is not untestable: `satisfied_rank` is `pub`, and `tessera-bench`'s
  `artifact_serving_scale.rs` calls it directly and even differentials it against the partition
  (`:1224`). But that is a benchmark binary the gate never runs, so **the only exercise of a
  disclosure-relevant path lives outside the test suite**. Under the builtin plugin — the only one
  that exists today, there being no wasmtime host — the arm is unreachable end to end; the function
  beneath it is reachable directly, so this is *have not tested*, not *cannot test*.

- **`tests/fold.rs` failed once under load and did not reproduce** (Wave 0 verification), and R3
  could **not explain it by reading**. Ruled out: fixed paths, ports, environment variables, `/tmp`
  collisions, process-global state, `Once`/`static` in `tests/common` — every case builds its own
  `TempDir`. What remains is a wall-clock dependency (twenty `Instant` deadlines, 30–60 s), but the
  whole binary ran in 5.86 s for 28 tests on a busy machine, so the margin is wide; and the reported
  form — a target failure naming no test — reads more like a harness-level abort than a deadline
  assertion, which would have named the case. Not reproduced, frequency unknown, deliberately not
  hammered. **Open.**
- **Three `fold.rs` cases share one deadline across sequential waits** (R3 F2, `:1266`, `:1410`,
  `:1571`), which the file's own `wait_for` doc at `:187` names as the thing not to do. They still
  catch their defects; a red one names the wrong step. Adjacent to the flake above without being
  shown to cause it.
- **A design silence, not a defect** (R2). A permissive shrink that empties a generating set does
  not reach a zero-credential principal — an artifact no visible member of which is in view is not
  a candidate — so the widened channel reaches only principals who see some member, C7 at its
  widest. But `annotation-write-cycle.md` §2.1 and C7 both describe the shrink as *"a principal
  satisfying the survivors"* and state no position on there being no survivors. The code is
  correct; the design does not speak to its own limit case.

## 9. What this campaign is expected to break

Stated in advance so that a surprise is recognisable as one.

- **A coverage row.** If a track finds that an invariant's cited evidence cannot fail, the honest
  outcome moves a row of `conformance.md` §4.6 from covered to uncovered. That is a loss on paper
  and a gain in fact, and `CLAUDE.md`'s rule governs how it is worded: *we cannot test this* and
  *we have not tested this* are different claims, and only the first is an excuse.
- **The gate's runtime.** Wave 0 added a `clients` job and a reachability check. The 94 s and 24 s
  figures were already stale and are now explicitly not a budget; if the gate becomes slow enough
  to be turned off, that is the campaign's doing and must be measured rather than guessed.
- **The assumption that a green suite means a tested system.** That is the campaign's whole point,
  and the fix wave is where it is paid for.
