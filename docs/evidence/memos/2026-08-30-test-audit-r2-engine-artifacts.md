# Test audit R2 — the engine's artifact and annotation surface

**Status:** Evidence — never normative. Taken at `2bde89a6` on `main`. **This assesses tests, not
the code under them**: every finding below is a statement about what a test would or would not
catch, and where the code was read it was read only to decide whether a test could fail.
`conformance.md` §4.6 and `docs/artifact-delivery.md` are untouched — a row moves only when a test
moves with it, and no test moved.

Track R2 of a four-lens audit: reachability, discrimination, fidelity, absence. The reachability
baseline is [the Wave 0 memo](2026-08-30-test-reachability.md) and is taken as input rather than
re-derived.

## Results

**305 tests assessed** — 216 across the 28 integration binaries named in the brief, 89 in the
`#[test]` blocks of the nine `src/` modules that carry any. The surface is **healthy**, and more
so than the repository average: the differential files (`artifact_containment.rs`,
`artifact_filter_bit.rs`, `artifact_census.rs`, `artifact_tile_index.rs`) carry explicit
anti-vacuity guards — *"no artifact matched, so the check below is vacuous"*, *"this arm exists so
the next one is not vacuous"*, *"the fixture no longer exercises the scoping, so this test proves
nothing"* — and the refusal tests that matter most carry positive controls beside them. Nothing on
this surface is unreachable that should not be, and nothing is vacuous.

**Four findings, and they are all about the same shape of gap**: the generating set. Where a test
concerns membership, counts, ordinals, adoption coordinates or the served set, it is sharp. Where
it concerns `G` — the set containment is evaluated against — the assertions are made from a
full-coverage principal, and a full-coverage principal cannot tell a correct `G` from an empty one.
Three of the four follow from that; the fourth is the publish-time guard that keeps the same
condition out at the front door and cannot say which of four rules refused it.

- **F1** is confirmed by mutation: erasing a generating set instead of shrinking it passes
  `a_permissive_layer_shrinks_the_generating_set_at_the_fold_and_serves_again`, and passes every
  other test in `tessera-engine`'s artifact binaries and in `tessera-lifecycle`.
- **F2** and **F3** are absences: **I12's containment conjunct** and **I8's headline** each have no
  test anywhere in the repository that would fail if the claim were violated.
- **F4** is one under-discriminating refusal test.

**On I11 and I8's standing in `conformance.md` §4.6** (the L4 question the brief asks to be
answered plainly):

- **I11's "regression in coverage" reading is still true of the *within-request* half and is now
  understated for the other half.** The deleted pin tests covered the cross-request half, and on
  the artifact side that half has since been rebuilt and tested in substance: every row-space
  artifact this surface owns carries a validity coordinate and has a both-directions adoption
  test — the containment partition (`artifact_containment.rs:483`, `:571`;
  `artifact_fold.rs:372`, `:409`), the tile index (`artifact_tile_index.rs:661`, `:784`), the
  row-major membership column (`artifact_row_major.rs:815`), and the row form itself
  (`artifact_cache_cadence.rs`, six cases). `artifact_fold.rs:743`
  (`a_merge_that_renumbers_extent_rows_disturbs_no_artifacts_count`) drives a real merge that
  permutes row space inside a prefix and pins that no artifact's count moves — which is closer to
  I11's stated hazard than anything the pin tests did. What remains genuinely uncovered is the
  within-request clause — *a request resolves the segment-set version once and uses it throughout*
  — exactly as §4.4 records. **The row's position should not move on this memo's evidence**, but
  the reason it stands could be narrowed to the within-request clause.
- **I8's "a testing gap, not absent machinery" is correct, and the gap is wider than §4.6's
  parenthetical suggests.** §4.6 says *"the engine's own fold tests cover both arms in substance"*.
  They cover the **deletion** arms (strict withdraw, permissive shrink) — and F1 shows the
  permissive one does not discriminate. They do not cover I8's headline at all: nothing tests that
  a **growth** leaves a generating set alone (F3).

## Findings

### F1 — the permissive-shrink test cannot tell a shrink from an erasure

**Class:** under-discriminating (and, on its name, mis-named). **Severity: S1** — the property it
fails to check is containment, on the one control I3 exists to make conservative.

`crates/tessera-engine/tests/artifact_fold.rs:866`,
`a_permissive_layer_shrinks_the_generating_set_at_the_fold_and_serves_again`. The whole of its
post-fold assertion:

```
let served = artifacts_of(&engine);
assert_eq!(served.len(), 1, "the artifact is back");
assert_eq!(served[0].content, vec!["shipping and logistics"], "with its description, now generated from the surviving sources");
assert_eq!(served[0].masked_count, 299, "and one fewer member");
```

`artifacts_of` (`artifact_fold.rs:145`) authorises with `full_coverage_credential()` and there is
no second principal anywhere in the file. Containment against a full-coverage mask is satisfied by
**every** generating set, so the comment *"now generated from the surviving sources"* is the one
claim in the assertion block that nothing checks. The test cannot distinguish `G = {0..30} \ {7}`
from `G = ∅`, from `G = {0}`, or from any other set the fold might have written.

**What it would let through.** A fold that emptied `G` rather than subtracting the retired members.
Containment on an empty set is `0 == 0` (`crates/tessera-engine/src/artifacts.rs:812`–`:815`), so the corpus-derived text
would then serve to every principal who reaches the layer and can see one member of the artifact —
which is what `corpus_independent_content_is_served_to_everyone_who_reaches_the_layer`
(`artifact_content.rs:484`) exists to assert *is* the meaning of an empty set. The publish path
refuses an empty generating set on a corpus-derived layer (`artifact_content.rs:683`); the fold
path has no such guard, because it does not need one — it is correct
(`crates/tessera-lifecycle/src/membership.rs:63`, `andnot_inplace(retired)`).

**Confidence: high — mutation used.** In a throwaway worktree, `andnot_inplace(retired)` was
replaced with `clear()`. `a_permissive_layer_shrinks_the_generating_set_at_the_fold_and_serves_again`
passed; all 28 tests of `--test artifact_fold` passed; all 122 tests of `-p tessera-lifecycle`
passed. Nothing in the repository turned red.

**One thing the same probe established, which bounds the exposure** and is recorded here so it is
not re-derived: with `G` emptied by a fold, a **zero-credential** principal is still served nothing,
because an artifact no member of which the viewer can see is not a candidate at all
(`artifact_serving.rs:200`). So the widened channel reaches principals who can see *some* member of
the artifact but were never entitled to the deleted source — which is C7's channel at its widest
rather than a new one. The design (`annotation-write-cycle.md` §2.1, architecture Appendix C7)
describes the shrink as *"a principal satisfying the survivors"* and does not state a position on
the case where there are no survivors; that is an observation for the owner, not a finding.

**The fix is one credential.** Re-assert with `subset_credential()` and a generating set drawn so
that the survivors are outside it, which the fixture already supports.

**Disposition:**

### F2 — I12's containment conjunct has no test: no filter is ever applied to a layer carrying a generating set

**Class:** missing. **Severity: S1** — an invariant this surface owns, with no test that could fail.

`conformance.md` §4.4 states what survives of I12 on the artifact side: *"an artifact's existence
verdict, its masked count and its containment are identical with and without any filter, since both
tests run against `M_auth` alone"*, and records that the Python suite does not drive it. The engine's
own tests drive the first two and not the third.

`artifact_filter_bit.rs:236`, `a_filter_moves_neither_the_served_set_nor_the_masked_count`, is the
test that would carry it. It compares served keys and masked counts across filtered, unfiltered and
matches-nothing requests, over four principals and five viewports — sharp on both quantities it
names. But both layers it uses declare no supplied content (`BY_LIST`, `BY_RULE`, built by
`build_corpus_fixture_with_layers`), and the one case in the file that does publish content
(`a_label_carries_its_targets_bit`, `:398`) publishes it with an **empty** generating set —
correctly, since inherited content must not carry one (C28), and the comment at `:433` says so.

Every filtered request in the repository is in one of three files —
`artifact_filter_bit.rs`, `artifact_projection.rs:225`, `region_leaf.rs` — and **no artifact
published in any of them carries a non-empty generating set**. That is the claim to check, and it is
not the same as the layer declaring no supplied content: `artifact_filter_bit.rs:374` *does* declare
a `supplied` field, and the distinction is between a layer's content schema and a published
artifact's `generated_from`. The two other files publish only through
`IncomingArtifact::from_entities`, which sets `contents: Vec::new()`
(`tessera-lifecycle/src/membership.rs:309`), so no content and therefore no generating set exists to
contain. No test in `tessera-server` combines the two either. So containment is
vacuous (`Containment::NothingToContain`) in every request that carries a filter, and the conjunct
is never evaluated under one.

**What it would let through.** A regression composing the filter into the mask handed to
`ArtifactRows::satisfied_rank` — `crates/tessera-engine/src/artifacts.rs:2173`, `self.mask` — so that containment failed
for a viewer whose filter excluded a generating-set member. The symptom is an artifact and its
description appearing and disappearing as a viewer types, which is the exact failure the file's own
module doc says it exists to prevent, one field further in than it reaches. The direction is
fail-closed rather than fail-open, but I12's statement is an equality in both directions and the
serving path's behaviour under a filter would be a function of the filter.

**Confidence: high — read only.** The absence was established by grep across every test in the
workspace that sets `request.filter`, then by reading each of their layer declarations.

**The fix is one layer.** Give `artifact_filter_bit.rs` a third layer with a corpus-derived supplied
content whose generating set is drawn across the filter's value, and add the served rank to the
`served()` tuple so the existing filtered/unfiltered equality covers it.

**Disposition:**

### F3 — I8's headline has no test: no growth is ever applied to an artifact carrying a generating set

**Class:** missing. **Severity: S1** — an invariant with no test.

**I8** is *"a label's generating set is immutable once supplied — items arriving later are not part
of it and must not be added"*, and `conformance.md` §4.6 records it as untested machinery whose
behavioural form is available and unwritten, adding *"the engine's own fold tests cover both arms in
substance"*. The fold tests cover the two **deletion** arms. The arm I8's own body names —
later arrivals are not added — has no test at either entry point.

`artifact_growth.rs` is the file that owns the arrivals, and its layer declaration
(`artifact_growth.rs:52`) is `supplied: Vec::new()`: not one of its fifteen cases publishes an
artifact with a generating set, so not one of them can observe whether a growth touched it. The
same holds for the two mint paths (`artifact_growth.rs:477` onward) and for the ingest-side join in
`artifact_interleavings.rs`.

**What it would let through.** `ArtifactStore::grow` (`crates/tessera-lifecycle/src/membership.rs:906`)
is one line, `record.members.or_inplace(joining)`, and a plausible change — keeping the description's
provenance "in sync" with the membership it describes — is one more line beside it. The result is a
label served on a set it was not generated from, and it fails **conservative**ly at first (a wider
`G` is harder to contain) before the caller regenerates against it. Low likelihood; the point is
that the invariant's own arm has nothing standing on it.

**Confidence: high — read only.** The mechanism is small enough that reading it settles the
question; no mutation was warranted.

**The fix is one case.** Publish a described artifact, grow it with a member outside the generating
set, and assert that a principal who satisfies the original `G` but not the newcomer still reads
the content. That test fails if anything ever adds to a set, and it is the behavioural form §4.1
says is available and unwritten.

**Disposition:**

### F4 — the four content-declaration refusals are asserted as one undifferentiated `is_err()`, with no positive control

**Class:** under-discriminating. **Severity: S3.**

`crates/tessera-engine/tests/artifact_content.rs:636`,
`content_that_disagrees_with_the_declaration_is_refused` — *"the four ways a batch can disagree with
what its layer declared, each refused at publication"*. Four `assert!(publish(…).is_err())` at
`:659`, `:666`, `:680` and `:691`, no message and no kind checked, and three of the four go to the
same layer `topics/a` with no successful publish to it anywhere in the test.

Every case that names a *rule* is therefore satisfied by any refusal at all, including one that has
nothing to do with the declaration — a `topics/a` that failed to register usably would make three
of the four pass, and the closing `artifacts_of(…).is_empty()` is consistent with that too.

The fourth case matters more than the other three: *"no generating set on corpus-derived content —
the one that would otherwise serve to everyone"* is the guard that keeps F1's empty-`G` condition
out at the front door. Its identity is not pinned by anything.

**What it would let through.** A refusal that fires for the wrong reason, or a rule silently
subsumed by an earlier check. The engine already returns four distinct, quotable messages — verified
by instrumenting the test in a worktree; each names its own rule, e.g. *"carries no supplied content,
and this layer declares 1 kind(s)"* — so `assert!(message.contains(…))`, in the form
`the_build_refuses_a_label_that_attaches_to_nothing` (`artifact_build_time.rs:519`) already uses, is
available at no cost.

**Confidence: high — instrumentation used** (the four error values were printed, not inferred).

**Disposition:**

## What was checked and found sound

Recorded so the attacks are not re-run.

- **`artifact_containment.rs` in full.** The strongest file on the surface. The differential runs
  both faces of the partition (`SMALL` settles its expression table, `LARGE` evaluates per
  candidate), the generating sets are drawn **across** pseudo-random signature groups rather than
  inside one, `UNPROJECTABLE` plants members that carry terms so that the projection-loss check
  cannot pass by the expression failing first, and `the_composed_expression_is_the_members_own_signatures`
  (`:621`) drops one depended-on term per case so the agreement is about *which* terms and not how
  many. The adoption rule is tested in both directions and against a level version *below* the
  composed one as well as above (`:483`) — the case a `>=` would admit.
- **Every row-space artifact's validity coordinate.** Partition, tile index, row-major column and
  row form each have an adopted-here / not-adopted-there pair, and the partition also has a
  cross-prefix case (`:571`). A defect relaxing any of those coordinates turns a named test red.
- **`artifact_filter_bit.rs`'s two quantities**, and its anti-vacuity guards (`:212`, `:217`), which
  are the reason the oracle equality means something.
- **The label-gating chain in `artifact_edges.rs`.** Suppression of an artifact, of its layer, and
  a layer gate the viewer fails, each checked on the viewport route *and* on a `tessera_id` taken
  before the suppression — which is the route that traverses no edge and the one a cached
  reachability would let survive.
- **I3's containment cases in `artifact_content.rs`** (`:408`, `:444`, `:484`) — three principals,
  a narrow principal who sees most of the corpus and still fails, and the ranked fallback. These are
  the cases F1 shows are missing on the fold's side of the same property.
- **The fold's Rule S / Rule F distinction** (`artifact_fold.rs:277`, `:1280`, and the file's module
  doc) and `a_merge_that_renumbers_extent_rows_disturbs_no_artifacts_count` (`:743`), which drives a
  real merge with four extents rather than asserting the premise.
- **`derived.rs`'s 28 geometric property tests.** Containment, simplicity, order-independence,
  budget exhaustion and degenerate inputs, each with a guard that fails when the fixture stops
  exercising the property (`:2224`, `:2328`, `:2777`).
- **The eight `#[ignore]`d measurement harnesses** (`hull_geometry.rs` × 7,
  `hull_triangulation.rs`; plus `membership_column.rs`'s `measure_the_column_cost`) were read for
  correctness claims that exist only inside them. **There are none** — every assertion in them is a
  threshold on a measured cost or a shape statistic, and each is a legitimate measurement rather
  than a silenced check, which is what Wave 0 also found.
- **`one_publisher.rs:18`** greps `crates/tessera-engine/src/session.rs` for two markers and would pass if publication
  moved to a third file. Not reported as a finding: its own doc says it is a second statement of
  `scripts/check-layers.sh`'s rule, and the shell rule is the enforcement.
- **`artifact_tile_index.rs:516`'s closing block** claims *"the growth moved the level's version and
  the whole family is rebuilt under one key"* while asserting only that a first build after the
  growth is correct. Not reported: the property that paragraph names is covered directly and in
  both directions by `artifact_cache_cadence.rs` and `artifact_row_major.rs:815`.

## Method

Every test named above was read against the design section it claims to cover —
`annotations.md`, `annotation-representation.md`, `annotation-write-cycle.md` §2.1 and §3.2,
`artifact-serving-at-scale.md` §4.1, architecture §4 (I3, I8, I11, I12) and Appendix C7. Mutation
was used twice, both in a throwaway worktree at `2bde89a6` that was removed afterwards, and nothing
was committed: once to confirm F1 (`andnot_inplace` → `clear`, running only `--test artifact_fold`
and `-p tessera-lifecycle`), and once to instrument F4's four refusals. The whole workspace suite
was not run, two other audit tracks being live on the same machine.

## Appendix R — review trail

- **r1 (2026-08-30)** — first issue. Track R2 of the 2026-08-30 test-quality audit campaign, over
  `crates/tessera-engine`'s artifact and annotation surface at `2bde89a6`. Four findings, three of
  them one shape of gap; F1 confirmed by mutation, F4 by instrumentation, F2 and F3 by reading. No
  live defect in shipped code found: the fold's shrink, the containment evaluation and
  `ArtifactStore::grow` were each read and are correct as specified.
