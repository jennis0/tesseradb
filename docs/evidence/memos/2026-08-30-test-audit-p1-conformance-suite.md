# Test audit P1 — the conformance suite (`conformance/tests`, `conformance/suite`)

**Status:** Evidence — never normative. Base commit `9279f49a` on `audit/test-quality`. This
assesses **the suite, not the system under it**: nothing below is a claim that shipped behaviour is
wrong, and no defect in shipped code was found. Track P1 of Wave 2 of the test-quality campaign
([`test-audit-campaign.md`](../../test-audit-campaign.md)). `conformance.md` is untouched — §4.6's
rows move only when a test moves with them, and no test moved; the two register corrections below
are for the owner to make or decline.

## Results

**Where the suite runs, it compares what it claims, and a masked-path defect would be caught.**
Every differential family I traced has a live way to fail, and the sharpest of them fail against
*modelled* defective engines rather than against argument: the I7 differential must disagree with a
first-*k* storage-order stub on more than half the tiles; the overlay differential builds both the
pre-overlay sampler and the pre-overlay θ anchor out of the oracle and requires the comparison to
reject each on more than half the tiles, and separately requires no denied `tessera_id` anywhere on
the wire; the canary comparator must reject a visible-items state **on every surface it compares**;
the byte-scanner is handed a plant per sweep mechanism and a clean payload per sweep mechanism. The
answer to question 2 is yes for every row §4.6 records as covered: an engine that counted over
everything and filtered afterwards moves the served *set*, and every one of these families compares
the served set, ordered, against a definition derived from fixture inputs rather than from the
artefact.

**The suite is not green.** Measured at the base commit, `pytest conformance/tests
conformance/suite`: **625 passed, 4 skipped, 7 errors** in 299 s. All seven errors are one module —
`conformance/suite/test_total_verification.py`, the correctness suite's §9 mechanism — whose
module-scoped fixture cannot build its bundle. It has been in that state since 2026-08-21, so the
row half, the census half and **both** of its negative controls have contributed nothing for nine
days, and CI's `conformance` job fails for the same reason. `conformance.md` §0 reports "432 of 432"
(the collected total is now 636: 442 in `tests`, 194 in `suite`). That is F1, and it is the finding
this track exists to produce: the deliverable's own gate is red while the register says green.

**The fixture-assumption class did not recur.** §0's two causes — `public` at term 0 and decision
0073's Morton tiebreak — are genuinely fixed at the sites that consume them, and the modules that
join a served row back to a planted value now go through `entity_of_source`/`source_of_entity`
rather than an equality (`test_mask_catalogue.py:211`, `conftest.py:85`,
`test_filter_differential.py:160`, `test_record_blob.py:108`). One echo of the retired equality
survives in prose only (F5). What replaced the missing checks is the right shape: fixture
preconditions are asserted *before* the thing that rests on them, and several of them are the best
work in the suite — `test_the_filter_columns_are_decorrelated_from_the_grant_structure`,
`test_the_text_column_has_the_shapes_its_catalogue_entries_need`, and the label fixture's
one-entity-apart check.

**`conformance/suite`'s shared machinery is sound, on the same reasoning R8 applied to the Rust
harness.** `canonical.py` removes exactly two sources of legitimate variation (the trailer's two
elapsed-time fields, tile emission order) and nothing else, keeps the surfaces separately
addressable so a difference is reported at the surface it landed on, and refuses rather than
canonicalises a malformed body. `entitlement.diff` explicitly declines to launder: a recording that
changed with no row appearing or vanishing returns `Unexplained`, which equals no entitlement. It
normalises nothing a test should have seen.

**Five findings: two S3, three S4.** No test that cannot fail was found inside a running module, no
`assert True`, no self-comparing assertion, no commented-out test and no swallowed exception outside
three legitimate `except OSError: pass` cleanups in the driver.

## Findings

### F1 — `conformance/suite/test_total_verification.py` has not run since 2026-08-21

**Claim:** the module's corpus shim writes a file set that the generator's own declaration has
outgrown, so `tessera build` fails in the module-scoped fixture and all seven tests — including both
negative controls — error rather than run.

**Evidence.** Reproduced at the base commit from the fixture's own inputs:

> `build FAILED: …/corpus/flat_artifacts.parquet: No such file or directory (os error 2)`

`conformance/suite/verification.py:271` runs `tessera build` with `check=True` and
`capture_output=True`, so the reason is swallowed and pytest shows only a `CalledProcessError`. The
declaration the build reads is `Corpus::config_toml()`, which since
commit `08e8a985` (2026-08-21, "the artifact arm reaches disk") carries five
`[[layer]]` blocks whose `source` names `flat_artifacts`, `flat_members`, `partition_artifacts`,
`partition_members` and `boundary_artifacts` (`crates/tessera-corpus/src/materialise.rs:157`). The
shim at `verification.py:133` writes `points.parquet`, `pairs.parquet`, `config.toml` and
`ingest.arrows` and nothing else; `Corpus::write_artifact_fixtures`
(`materialise.rs:587`) is what writes the other five and the shim never calls it. The declaration's
own comment still says those sources are "unreferenced by every column above — a source nothing
names is a name in a table, not a file this build has to open", which was true when written and is
no longer, because layers name them.

**The drift has a named cause**, and it is F5's class: `verification.py:37` states "the CLI carries
no verb that writes them: `tessera corpus` has `items` and `census` only", which is why the shim
exists. `tessera corpus materialise` has written exactly this fixture — declaration and all four
artifact files — since the same 2026-08-21 commit. Two writers of one input set, one of them told by
its own module doc that the other does not exist.

`verification.py` and `test_total_verification.py` were last touched on 2026-08-18, three days before
the generator moved; the break is inherited from `main` and is not this branch's (`git diff
$(merge-base) HEAD -- conformance/` is empty).

**Class:** unreachable. **Severity:** S3.

**What a defect would let through.** Everything §9 verifies: every served row of every recorded
response against `tessera corpus items`, and the per-tile masked census per principal against
`tessera corpus census` minus the harness's own denies. The census half is a per-principal masked
count, so its darkness is adjacent to the disclosure surface — but the property is covered in other
forms elsewhere (the viewport differentials, the catalogue), which is why this is S3 rather than S1.
The second-order cost is larger than the first: the module's own negative controls are dark too, so
nothing would notice if the mechanism came back wrong.

**Confidence:** high — measured, reproduced from the CLI, and the cause read in both trees.

**Disposition:**

### F2 — the I3/I12 trip-wire watches a route that will never exist

**Claim:** `test_i3_has_no_surface_to_test` pins the absence of a label surface by probing
`/v1/labels`; the surface landed as the viewport's artifacts frame plus `POST
/v1/artifacts/{tessera_id}`, so the pin can never fire, and the obligation it defers is writable
today and unwritten.

**Evidence.** `conformance/tests/test_filter_differential.py:710`–`:731`:

> there is no label service, no `/v1/labels` route, no generating sets and no frontier … This test
> pins the *absence* instead, so the gap cannot rot silently: the day a labels route answers
> anything but 404/405, this fails, and whoever lands it owes the I3 half of this differential — a
> filtered request whose label set equals the unfiltered request's.

Every clause of the premise has been false since r13. `crates/tessera-server/src/viewer.rs:50`
routes `/v1/artifacts/{tessera_id}`; artifacts ride the viewport response as their own frame
(`oracle.wire.decode_viewport_artifacts`, used by `test_label_containment.py:41`,
`test_shape_membership.py:51` and `test_region_leaf.py`); generating sets are built and containment
is enforced. `conformance.md` §4.6 already records I3 as covered by `test_label_containment.py` and
I12's artifact half as "built and blind to the filter by construction, and undriven here" — so the
matrix is right and only this module is stale. The module doc repeats the premise at `:35`–`:38`.

**Class:** mis-named (the pin is also vacuous — a 404 on a route nobody will implement is
unfalsifiable). **Severity:** S3.

**What a defect would let through.** Nothing directly: the test asserts a 404 and gets one. The cost
is the one the test was written to prevent — the I12 obligation it defers has no other trip-wire, so
"a filtered request's artifact verdicts, masked counts and containment are identical to the
unfiltered request's" (architecture §8.4) can stay unwritten indefinitely while a reader greps `I3`
in this module and finds a green test saying there is nothing to test. The check itself is now
cheap: request the same viewport with and without `SWEEP_EXPR` against a layer-bearing fixture and
compare the artifacts surface, which `suite.canonical` already isolates.

**Confidence:** high.

**Disposition:**

### F3 — the endurance tier's 1,145 lines are executed by nothing

**Claim:** `conformance/suite/test_endurance.py` collects three tests; the one that runs the
machinery is gated on an environment variable no gate sets, and the two that do run check the
env-var parsing rather than the tier.

**Evidence.** `conformance/suite/test_endurance.py:845` gates `test_endurance_long_life` on
`TESSERA_SUITE_ENDURANCE=1`; `.github/workflows/ci.yml:154` runs plain `pytest conformance/tests
conformance/suite`, and no other invocation sets it (`grep -r TESSERA_SUITE_ENDURANCE` finds the
module and its own doc). The two collected siblings are
`test_the_defaults_are_the_design_tier` and `test_the_verify_cadence_grammar`. The design's own
position is that this is right — "**This is a backstop, never a gate** (§6, §13) … wiring it into a
gate and then disabling it is the failure that sentence in the design exists to prevent" — and the
finding is not that it should be gated. It is that nothing establishes the tier still *runs*: the
release tier that would exercise it does not exist (`conformance.md` §6: "nightly and release do not
exist"), so the module has drifted against the driver for as long as it has existed and the first
person to run it discovers that instead of an endurance result.

**Class:** unreachable. **Severity:** S4.

**What a defect would let through.** Nothing today — it asserts nothing today. What it costs is that
the backstop is unproven at the moment it is wanted, which is before a release. A cheap standing
answer is a smoke parameterisation (`TESSERA_SUITE_ROUNDS` small) run somewhere other than the PR
gate; I did not run one, and say so under *what I did not cover*.

**Confidence:** high — measured (the full run's fourth skip is this test).

**Disposition:**

### F4 — the `artifacts` canonical surface is declared as compared and is exercised by nothing

**Claim:** `suite.canonical` makes artifacts one of five separately-addressable surfaces and argues
at the site that it must be compared; the canary comparator iterates four of the five, the
canonicaliser's own per-surface control builds no artifact frame, and `entitlement._analyse_viewport`
gives it no analysis — so the argument holds for four surfaces and not for the fifth.

**Evidence.** `conformance/suite/canonical.py:91`:

> It is compared all the same: without it, two responses differing only in which clusters they
> served would canonicalise identically, and a determinism break in the artifact channel would pass
> every comparison in the suite.

`conformance/tests/test_canary.py:184` is `SURFACES = ("tiles", "points", "underlay", "trailer")`,
and `compare_states` iterates exactly that tuple — so the I2 comparator does not read the artifacts
surface. `conformance/suite/test_canonical.py:167` pins the five names on the stated ground that
"the canary's per-surface control assertions key off them", which they do not; the same file's
`test_each_difference_lands_on_exactly_its_own_surface` moves tiles, points, underlay and trailer and
constructs no artifact frame anywhere in the module. `entitlement.py` names every other surface in
its per-surface reasons and never names this one.

**Class:** mis-named. **Severity:** S4.

**What a defect would let through.** A determinism or I2 defect confined to the artifact channel is
invisible to the canary comparison — which is the bound `conformance.md` §4.6's I2 row already
states in as many words ("served artifacts travel on their own frame with a masked count, derived
geometry and content, and the comparator does not read it"), and the reason this is S4 rather than
S3. Two things bound it further: `entitlement.diff` compares whole `Streamed` values, so an
artifacts-only change in a stage recording surfaces as `Unexplained` rather than as silence; and the
channel's *values* are compared against independent oracles in `test_label_containment.py`,
`test_shape_membership.py` and `test_region_leaf.py`. The gap is the canary fixture having no
layers, not the tuple; adding "artifacts" to `SURFACES` today would fail the positive control
correctly, since there is nothing to differ.

**Confidence:** high.

**Disposition:**

### F5 — four claims in the corpus and the suite that a reader would act on and that are no longer true

**Claim:** four load-bearing statements about the suite are stale in the direction `CLAUDE.md`
singles out — machinery recorded as absent that exists, and a marker recorded as standing that was
removed.

**Evidence.**

- **`fx_key` is served, and three sites say it is not.** `conformance.md` §2's ⊘ ("The scalars are
  planted, and the points batch does not serve them … pinned by a **strict xfail**"), §4.2's
  built-marker ("`fx_key` remains planted and unserved, and its strict xfail … remains the marker")
  and §7's decision 4 all stand; `conformance/tests/test_mask_catalogue.py:178` records "The xfail
  was removed on 2026-08-07, when `tessera build` gained `--schema`", and
  `test_fx_key_is_served_in_the_points_batch` asserts the served column and joins every served point
  back to its planted key. `test_canary.py:41` repeats the retired marker too. Three differential
  modules already depend on the served column (`_served_entities` in the filter, keyword and text
  differentials), so the doc understates what the suite rests on.
- **`verification.py:76` says a wire defect is "pinned as a strict xfail in
  `test_total_verification.py`".** There is no `xfail` in that file;
  `test_the_points_tail_is_named_by_its_render_declaration` is a plain assertion — and under F1 it
  does not run, so whether the defect it describes still stands is presently unknown.
- **`verification.py:37`'s "the CLI carries no verb that writes them"** — `tessera corpus
  materialise` has written exactly that fixture since 2026-08-21. This is the premise F1's break
  grew in.
- **`test_stage_invariance.py:43` states "the catalogue's entity id *is* its source id"** — decision
  0073 retired that equality, and it is the assumption r15 spent a rework removing. The **code** is
  correct: `_SOURCE_OF_FX` at `:96` maps `fx_key → source id` and the external id is built from the
  source id at `:193`, which is the right join. Only the sentence is wrong, and it is the sentence
  the next person will read when the join looks suspicious.

**Class:** mis-named. **Severity:** S4.

**What a defect would let through.** Nothing on its own. The cost is the one `CLAUDE.md` names: a
register that records built machinery as absent teaches a reader to discount it, and F1 is what
happens when two components are each told the other does not exist.

**Confidence:** high — each checked against the code that contradicts it.

**Disposition:**

## Which differential families can fail, and how

| Family (module) | What makes it fail | Verdict |
|---|---|---|
| I7 selection (`test_i7_selection.py`) | served set compared as an **ordered list** against `oracle.viewport`'s §7.2, per case × depth × `k`, θ saturated and live; `K_max` read from `/v1/meta` and asserted against the configured value first | **live, and the strongest in the suite** — negative control requires disagreement with a first-*k* stub on > half the tiles; a truncation guard fails any case whose comparison degenerated to "serve everything visible" |
| I1 overlay (`test_overlay_journal.py`) | composed counts *and* served points after ~2,400 acked denies under live θ; no denied `tessera_id` anywhere in the payload | **live** — two defective engines modelled from the oracle (pre-overlay sampler, pre-overlay anchor, plus a count-matched variant) and each required to differ on > half the tiles. Bound stated at the site: only the subtracting arm is reachable, so an engine composing by subtraction alone would pass |
| I2 canary (`test_canary.py`) | canonicalised bytes per surface across three states × three grant sets × seven zooms | **live** — the visible-items state must differ on **every** compared surface, and the untruncated premise is asserted per tile. Bounded to four of five surfaces (F4) |
| I10 byte scan (`test_byte_scan.py`) | any encoding of an entity id, the identity key or a misplaced external id in wire, sub-cell stream or logs | **live** — a plant *and* a clean payload per sweep mechanism, plus a real-traffic control that recovers a genuine `tessera_id` |
| I3 containment (`test_label_containment.py`) | `l-whole` absent whole for the narrower principal, `l-ranked` falling back, `l-core` served to both in the same response; four zoom tiers; cache half over three warming orders | **live** — the one-entity difference is asserted from the engine's own masked counts first, and the post-suppression response must equal the narrower principal's byte for byte |
| I12 mask half (`test_filter_differential.py`) | served set **equal** to brute-force `M_sel` over fixture-planted values; `visible` unmoved per tile; C11's five spellings byte-identical | **live** — decorrelation of attribute from grant is a checked precondition, the θ-live case has its own control, and the single-member positive control stops "identical and empty" passing. Artifact half undriven (F2) |
| keyword / text (`test_keyword_*`, `test_text_*`, 318 cases) | engine's dictionary-and-postings answer against an oracle holding only planted strings; per-layer ordinal traps built so a wrong resolve returns the **wrong** entities rather than none | **live** — fixture-shape preconditions assert every claim the matrix rests on (carrier counts, both adjacency shapes, a long singleton tail, entities with no value at all) |
| shape membership (`test_shape_membership.py`, `_wgs84.py`) | independent even-odd walk over quantised source geometry, at the build, after a flush and after a fold; every served point's `membership:` column | **live** — the WGS84 module additionally computes the *rejected* chord reading and requires the service to disagree with it |
| region leaf (`test_region_leaf.py`) | lasso, box and artifact operands against `oracle.filters`' own walk under three principals; `none_of` complement; cover verdict a superset | **live** — unknown, suppressed and withheld artifacts compared frame-for-frame and header-for-header |
| schema refusals (`test_schema_refusals.py`) | non-zero exit **and** the message naming its authority | **live** — positive control builds the same corpus under a well-formed schema and requires the blob's base files |
| restart replay (`test_restart_replay.py`) | acked denies and the allocator high-water after truncation to the published prefix; `discarded == 0` checked *before* truncation; the sidecar required to advance | **live within a bound it states first** — the module doc opens by saying neither test falsifies ack-before-fsync, measured by nulling `sync_data()` |
| record blob (`test_record_blob.py`) | `self_check` returning failures; the oversize rule on the artefact | **pass-only on the walk** — nothing feeds it a mis-addressed blob (P2 found the same from the other side). The oversize and placement tests around it are real |
| mask catalogue (`test_mask_catalogue.py`) | `verify()`'s drift report, including check 3b's item-by-item space comparison; a pairs-derived mask per case | **live**; `verify_identity_cross_check` remains pass-only (P2's list) |
| stage invariance (`suite/test_stage_invariance.py`) | `diff(before, after) == entitlement` over 20 stages | **live** — four negative controls, including a tampered points surface that must come back `Unexplained` |
| crash atomicity (`suite/test_crash_atomicity.py`) | a kill at three publication seams must land on an endpoint, never between | **live** — a control asserting a landing between the endpoints is refused, and another that a kill lands only on a seam |
| diff routes / canonical / battery / entitlement | the two diff routes must agree row for row, including on nulls, reorders and duplicate identities | **live** — the randomised generator is the half that matters, and the canonicaliser's chunk-boundary sensitivity is asserted rather than assumed |
| server profiles (`suite/test_server_profiles.py`) | the same plan walked per profile, each stage checked against its entitlement | **live** — an out-of-memory control contrives a kill at 8 MiB and asserts it is classified as a resource outcome, not a correctness failure |
| total verification (`suite/test_total_verification.py`) | — | **does not run** (F1) |
| endurance (`suite/test_endurance.py`) | — | **does not run** (F3) |

## What was checked and found sound

- **The controls are demonstrations, not claims.** Three modules build a wrong engine and watch the
  comparison reject it (`test_i7_selection.py:339`, `test_overlay_journal.py:392`,
  `test_canary.py:248`), and each states a threshold with a reason why a weaker one would be a
  coincidence. This is the single healthiest property of the estate.
- **Non-vacuity guards are pervasive and specific.** `truncating_tiles > 0` per selection case,
  `require_partial`'s sibling in the overlay differential, `capped_tiles > 0` for `K_max`,
  `subcell_rows_seen > 0` in the byte scan, `assert served_ids` after the denial sweep,
  `assert points.num_rows > 0` before an `fx_key` join, `assert served` before iterating artifacts.
  Where a comparison could degenerate to "both sides empty", something usually says so.
- **The catalogue is protected against becoming un-adversarial.**
  `test_the_catalogue_covers_the_properties_the_design_names` writes the expected list out rather
  than importing it, and argues at the test why importing it would be tautological.
- **Fixture isolation is thought through and stated.** `conftest.private_catalogue_bundle` exists
  because an accepted deny is published *into the bundle prefix*, so a private cache and WAL isolate
  nothing; one copy per server, not per module, because two servers sharing a copy compose each
  other's denies. Modules that mutate spawn their own.
- **`entitlement.diff` refuses to go soft.** Truncation is decided per tile from the response
  itself, never from configuration; when no untruncated surface remains the result is `Uncheckable`
  — equal to no entitlement — rather than a count comparison presented as the full check.
- **`suite.canonical` normalises exactly two things** and pins its own assumption
  (`test_the_points_comparison_is_sensitive_to_chunk_boundaries`), so a timing-based flush would
  fail a test instead of surfacing as an unreproducible flake.
- **Honest self-limitation, repeatedly.** `test_restart_replay.py` opens with what it does *not*
  prove, measured by nulling `sync_data()`; `test_crash_atomicity.py` states that fsync ordering is
  not observable through the API; `test_overlay_journal.py` states that a subtraction-only engine
  would pass. None of these is hedging: each is a bound a reader needs.
- **No `assert True`, no self-comparing assertion, no commented-out test, no bare `except`** in
  either tree; the three `except OSError: pass` sites in `driver.py` are directory cleanup and a
  cgroup file that may not exist.

## Two register corrections, for the owner

**`conformance.md` §0 records as absent two pause points that exist, are named identically, and are
driven from `conformance/`.** §0's table says "Interleaving scripts … **zero**. No `conformance`
feature, **none of the eight pause points**", and §5 predicts the shape of the eventual correction:
"the two designs share no vocabulary". They now share it exactly.
`crates/tessera-lifecycle/src/faults.rs:242` names `after_fsync`, `before_ack`,
`before_manifest_publish`, `before_current_flip` and `before_merge_publish`; three of §5's eight
exist there (`after_wal_fsync` under a shorter name), and
`conformance/suite/test_crash_atomicity.py:136`, `:183` and `:230` arm `before_current_flip`,
`before_manifest_publish` and `before_merge_publish`, kill the server at each, apply §12.3's
per-seam discard rule and check the landing against an entitlement. That is not one of the eight
*scripts*, and none of them is written — but "none of the eight pause points" is measurably false,
and the machinery §5 says the stage must extend rather than duplicate has been extended, by the
correctness suite, in `conformance/`. Whether §0's row moves is the owner's call.

**The counts in §0 and §1 have drifted again.** 432 → **636** collected (442 in
`conformance/tests`, 194 in `conformance/suite`), from 151 test functions across 26 modules. P2
recorded the seventeen-vs-seven module count; this is the case count beside it, and §0's "the suite
running green: 432 of 432" is also the claim F1 contradicts.

## What this track did not cover

- **`conformance/suite/test_endurance.py`'s 1,145 lines were read for structure, not audited.** I
  did not run a reduced-parameter smoke, so I cannot say whether the tier still executes — which is
  F3's whole point, and running it would have taken a corpus build of ≥ 60,000 items plus a
  multi-hundred-stage walk.
- **`test_total_verification.py`'s seven tests were read but could not be observed** — under F1 the
  fixture never reaches them, so nothing here says whether the row half, the census half or their two
  negative controls work when the build is fixed. That is the second-order cost F1 names.
- **No mutation was performed.** Reading settled every question I formed, per the campaign's owner
  ruling of 2026-08-30, and the modules that would most repay mutation carry their own modelled
  defective engines already.
- **`test_keyword_layers.py` and `test_text_layers.py` (124 cases) were read at the level of their
  module docs, test names and the two per-layer trap constructions**, not line by line. Their design
  — each layer's ordinal 0 planted so a cross-layer resolve returns the *wrong* entities rather than
  none — is the right shape, and I did not verify every probe in their matrices.
- **`suite/driver.py` (1,449 lines) was read at its seams** — the faults build, the barriers, the
  discard rules, the resource classification — and not exhaustively. Its ladder arithmetic is
  asserted by the plan's own barriers, which is the property that would catch a drift.
- **Timing and work-indistinguishability are out of scope by design** (Appendix C's C11 and C25;
  the probes own them), and I did not assess whether the probes discharge them.
