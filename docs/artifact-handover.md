# Handover — artifact work, after the scale investigation

**Date:** 2026-08-22 · **Status:** Stage 5 closed, the conformance suite green, Stage 6's cost
discussion held and measured — and **Stage 8's scale question measured, reviewed, and re-measured on
the corrected probe**.
Branch `artifacts/scale` (off `artifacts/stage-4`).

**Read [`artifact-delivery.md`](artifact-delivery.md) first** — it is the status record for all
artifact work by owner direction, not GitHub issues, and it wins over this document wherever they
differ. [`artifact-config-handover.md`](artifact-config-handover.md) is the configuration rework's
own handover and is still accurate about its surface; this one does not repeat it.

## 0. The scale investigation — read the memo, not this section

[`design/artifact-serving-at-scale.md`](design/artifact-serving-at-scale.md) is an **options memo for
owner decision**, backed by
[`probes/2026-08-20-artifact-serving-scale/`](../probes/2026-08-20-artifact-serving-scale/README.md).
One part is built and gate-green — the cut rewrite — because it was a data-structure choice rather
than a design one. Everything else is measured and proposed.

**The target's two axes are met one at a time.** At 10⁷ artifacts over 10⁸ points the whole grid of
principals by zooms runs 8.4–905 ms, and at 10⁶ artifacts over 10⁹ points 0.75–209 ms — both inside
the one-core one-second budget, against seconds per request for the shipped path. The corner where
both axes are large, 10⁷ artifacts over 10⁹ points, **exhausted this box and is modelled rather than
measured**. The memo's §7 carries the grids: six principal coverages by seven zooms, both axes swept
through their middles, medians of three runs.

Six things a reader needs before opening it.

- **The request path is `O(artifacts)` four times and only one of the four has the request in it.**
  That is the whole finding; the design separates them by cadence — request, token, generation — and
  gives each the layout that suits it.
- **There is no per-token structure.** An earlier draft leaned on `architecture.md` §8.5's
  servable-label set, cached per token; there are a great many tokens (owner, 2026-08-21), so that is
  the wrong cadence however cheap one copy is. Containment does not need it: `G ⊆ M_auth` is a
  boolean expression over **terms**, composable at build time, so artifacts sharing an expression
  share an answer for every principal that will ever exist. The per-token route is genuinely faster
  per request — 12.4 ms against 134 at 10⁶ artifacts — and costs 99–343 ms of setup per token per
  generation, 0.85–1.7 s at 10⁷, so thousands of near-unique tokens are gigabytes resident and minutes
  of rebuild at every generation move. That is the ruling's price, measured.
- **Cost should track what a viewer may see, and today it inverts.** At 10⁶ artifacts the shipped
  path's *worst* request is its narrowest viewer — 4 443 ms at a 3.1% mask against 1 349 ms at a full
  one — because a narrow `M_auth` is a more fragmented row-space set. The design's route inverts it
  back: 36.8 ms and 132 ms at the same two cells, and sub-millisecond as the narrow viewer's viewport
  closes.
- **Locality decides the layout, and residency decides it rather than speed.** A scattered layer has
  no row-space locality at all — every artifact is too wide for any node of the index — but on the
  corrected routes there is **no serving-speed wall up to the 10⁵ artifacts measured**, which
  supersedes the ~2×10⁵ wall this section used to report. What forces a **row-major** layout, a label
  per row where the layer partitions and a list per row where it overlaps, is that at 10⁹ points the
  artifact-major form does not fit: 4 GB against 78.5.
- **The cut is built, and it is the one finished thing here.** 1 008 ms → **3.05 ms** at 10⁷, by
  walking down from the roots instead of sweeping the level. Serves exactly what it served before,
  checked against the reference implementation over random trees.
- **What bounds a request is masked candidacy**, at every scale measured — 549 ms of the 553 a
  whole-map request costs at 10⁷ artifacts, and 242 of 307 at the nested arm's worst cell over 10⁹
  points. It is the test §4 requires and cannot be removed; what the design bounds is which artifacts
  pay it. On a real hierarchy the coarse node's masked count is second — 4.4 ms at a full mask, 49.5 ms
  below one, falling ~4× as the viewport narrows, measured at 10⁶ nodes — and per-signature counts
  would remove it for ~1.3 MB, scoped and unbuilt, with the storage to check against the census's
  54,794 signatures rather than the fixture's thirty-two.

**Two cautions about the numbers, both learned the hard way.**

**The fixture is the experiment.** Six corrections were needed and every one moved a headline further
than any option did — the last being that the treed arms' tree is unrelated to their geometry, which
is not a hierarchy, and which made the cut look like the bound when it is 0.3–11.4 ms on one. The
probe's fixture section lists all six; read it before trusting a figure.

**A grid whose extremes are cheap says nothing about its interior.** Sampling four viewports and
three coverages understated the worst request by 2.1× (owner). Sweep both axes through their middles.

**The review has run, its two fail-opens are amended, and the re-measurement it required is
complete** (review 2026-08-21, [the record](evidence/memos/2026-08-21-artifact-serving-scale-review.md);
re-measurement 2026-08-22, in the memo's §7). Two headline sentences. **The omitted masked work was
real**: the whole-map cell at 10⁷ artifacts over 10⁸ points read 31.7 ms on the routes that skipped
it and reads **553 ms** with it, of which candidacy is 549. **The conclusion survives the
correction**: the design runs 10–25× ahead of the per-artifact loop, holds the one-core one-second
budget at every measured cell, and inverts the cost so that narrow principals are the cheap ones.

**And the campaign's largest design finding came out of the re-measurement rather than the review.**
The expression census counts 32 distinct containment expressions under `annotations.md` §8.1's
per-term authoring, and **one expression per artifact** for generating sets drawn across the demo
corpus's real 54,794-signature distribution. So the containment partition's wide-viewport union route
exists only for per-term-authored generating sets; for drawn sets containment degrades to a
per-candidate evaluation, which the viewport bounds — cheap at narrow zooms, and a cost the build
wave has to price at wide ones.

**The memo's §9 ruling is taken** (2026-08-21): every build reports blocks per artifact, the
row-major layouts are used wherever the layer partitions, and **there is no declared bound at all** —
not a refusal and not a warning key
([decision 0092](decisions/0092-the-build-reports-a-layers-shape-and-no-layer-carries-a-declared-bound.md)).
Two rulings landed with it: nothing is materialised per token over the artifact population
([0093](decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)), and the
serving layout is chosen automatically per (layer, level), pinnable per layer, and re-evaluated at
each fold ([0094](decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)).

### 0.1 What to do next, in order

**Nothing here is blocking and nothing is half-done** — the branch is gate-green and every claim is
either measured or marked. Take these in order; the reasons for the order matter more than the list.
**The order and its dependencies are now a plan of their own**:
[`2026-08-21-artifact-scale-plan.md`](evidence/memos/2026-08-21-artifact-scale-plan.md), which
carries the tracks, what each must prove, and the campaign matrix.

1. ✔ **The re-measurement is done** (2026-08-22) — the corrected routes, a decorrelated fixture, and
   medians of three runs, with the `nested` arm measured at 10⁶ nodes over both 10⁸ and 10⁹ points so
   §7's grids and the hierarchy section no longer have to be held at once. **Three things it leaves
   open, and each needs a machine or a run rather than a decision**: 10⁷ artifacts over 10⁹ points
   exhausted this 47 GB box and is modelled, the `nested` arm has never run at 10⁷, and the layout
   sweep's blocks = 8 point breaks the trend with no fixture explanation — so a targeted re-run of
   blocks 6/8/10/12 comes before any threshold is quoted.
2. **Fix the global store version** (memo §8.1) — **in flight** on `artifacts/cache-cadence`, with
   §8.2's per-generation lineage. Any artifact write invalidates every cached row form in every
   view, and the rebuild is **300 s at 10⁷ over 10⁹**. Nothing above is worth building on a cache
   that never survives a write, and the per-generation structures this design adds would inherit the
   same fate.
3. **Price per-signature counts for coarse nodes** (memo §7.3) — the hierarchy's second term after
   candidacy, and
   ~1.3 MB if stored only where the count is dear. **Check the storage against a real corpus's
   signature distribution first**: it scales with the signature count, not the artifact count, and a
   thousand signatures stored per node would be 40 GB at 10⁷.
4. ✔ **The §9 ruling is taken** (2026-08-21) — and it is *no bound at all*. The row-major layouts
   dissolved the motive; what is left un-helped is a layer that is scattered *and* overlapping *and*
   numerous, and that layer is **reported at the build** rather than bounded at the request
   ([decision 0092](decisions/0092-the-build-reports-a-layers-shape-and-no-layer-carries-a-declared-bound.md)).
   Two more rulings came with it — [0093](decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)
   and [0094](decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md).
   **The adversarial review over the ruled memo and
   [the selection surface](evidence/memos/2026-08-21-artifact-layout-selection.md) runs before the
   serving-path build starts.**
5. **Then build into the serving path**, in this order: the containment partition (§4.2), the
   hierarchical index and extents (§4.4), the row-major layouts (§5), and the selection surface over
   them. Each of the first three is measured probe-side and none is in `viewport.rs` yet.

**Only the cut is built.** `crates/tessera-engine/src/cut.rs` and `Lineages` in `session.rs` are in
the engine and gate-green; everything else lives in
`crates/tessera-bench/src/bin/artifact_serving_scale.rs` and is a measurement, not an implementation.

**Two habits this campaign paid for**, both in the probe's fixture section: the fixture is the
experiment — six corrections were needed and every one moved a headline further than any option did
— and a grid whose extremes are cheap says nothing about its interior.

## 1. Where the rest of the work stands

**Nothing on this list is blocking.** §1.1 is Stage 6, whose implementation is now specified enough
to start; §1.2 is closed and is here for what it taught rather than for what it owes.

### 1.1 Stage 6 — its cost discussion is held, and the answer is measured

**Do not start Stage 6 by writing code** was the owner's direction (2026-08-20), and the discussion
it called for is done. **A predicate layer's row form is cached and rebuilt at a generation move**,
exactly as an enumerated layer's is, so its per-request cost is Stage 2's and the predicate is
invisible to the serving path.

The measurement is
[`predicate_membership_cost`](../crates/tessera-bench/src/bin/predicate_membership_cost.rs) and it
is not close. Deriving per request by crossing the posting lists into row space — which is what the
filter surface's row route does for a leaf — costs **7 900×** a cached count at the demo corpus's
263 artifacts over the whole map: 6.9 seconds against 0.9 ms. A crossing's cost is
`rows × artifacts` where a cached count's is `containers × artifacts`; one crossing serves every set
(decision 0062's rule holds), but each set still costs a probe per row, and a layer is exactly a
collection of sets. The ~5% crossover that makes the row route right for a *single* leaf therefore
never arrives — the tenth-of-the-map column is that route's best case and still loses by three
orders of magnitude, and the gap **widens** with the layer — 3 700× at 64 artifacts, 83 000× at a
million, where it is eleven hours per request. **The caching arm's whole generation-move cost is
repaid by one request**, 15 ms at 263 artifacts.

Two things follow, and both are worth carrying into the implementation.

**The per-request bound is withdrawn** — it is not deferred, unspecified or owed
([decision 0092](decisions/0092-the-build-reports-a-layers-shape-and-no-layer-carries-a-declared-bound.md),
2026-08-21). What this section said before is worth keeping as the reasoning that got there: a bound
is about how much a client gets back where caching is about what the server spends getting it, so it
was never the cost control it looked like at the sizes a layer is normally published at. The
measurement then fixed two things about the answer, and neither of them was a bound.

*It has to bite well below a million artifacts — unless the counts come from the column.* At 10⁶
predicate artifacts over 10⁷ points the per-artifact loop costs **462 ms per request** over the
whole map and **15.6 s per generation move**, and the cut reduces neither — it runs after the
verdicts, so it serves fewer and evaluates exactly as many. A few thousand is comfortable: 22 ms at
1 024, 106 ms at 10 000. Memory behaves, 108 MB for that million.

*And a third route removes the artifact count from the request path, with no new storage.* A
single-valued attribute predicate **partitions** the corpus — the column's distinct values are its
artifacts and every point carries one — so the column already says which artifact each point belongs
to, and one pass answers every artifact at once. **It lives in `attrs/`, not the render table:**
`membership = { attribute = … }` names an *indexed* column, and an indexed column is a
`ValueColumn` — a dense typed code array plus a presence bitmap, addressed by entity — which is
exactly what `category_membership` already walks. Entity space is where the counting pass wants to
be anyway, a masked count being `|membership ∩ M_auth|`.

It is **flat in the artifact count** where the per-artifact loop is not, so the two cross: near
10 000 artifacts on a 10⁶-point corpus and near 30 000 on a 10⁷-point one. At a million artifacts
over 10⁷ points it is **175 ms against 462 ms — 2.6×**; over 10⁶ points, 8.6 ms against 68.5 ms.
Each wins on its own side, and **nothing on the wire names which route served**: both compute the
same quantities from inside `M_auth`. What the choice does reach is the timing channel, which the
register now annotates (architecture r49, C4 and C15 shapes). Answers asserted against the loop's
rather than assumed equal.

Two passes with different domains, and that is a disclosure rule rather than an optimisation — the
count is over the whole membership (`annotations.md` §4.2) so its pass walks the mask, while
candidacy is against the viewport. **The candidacy pass is the expensive half**: ~120 ms of the 175
at a broad viewport, one inversion per viewport row, against ~30 ms for the counting pass.

⊘ Three reductions are modelled, not measured — and **the first is now foreclosed**: the counting
pass is a function of `M_auth` and the layer rather than of the request, so it could be held per
session on the mask fragment's cadence, which
[decision 0093](decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)
rules out because it is sized by the artifact population and there are a great many tokens. The
other two stand: the
candidacy inversion is exactly what a **rendered** copy of the column would remove; and with counts
from the column, row forms are needed only for the artifacts actually served, which the budget
bounds — so the move cost becomes a handful of lazy projections. ⊘ It is for a single-valued
**category** predicate: a multi-valued column does not partition, a **keyword** column's ordinals
are per layer and would need each layer's dictionary to merge, and a spatial predicate has no column
at all though its Morton-range membership makes `range_cardinality` cheap per artifact anyway.

*And the column route is the general one.* This section used to end by requiring the bound to refuse
on the layer's **declared** artifact count rather than on the principal's visible one — which was
the right constraint on a bound and is moot now there is none. What replaced it: the scale campaign
found that what decides cost is **row-space locality** rather than the membership source, so the
column histogram above is one instance of a row-major layout that applies to enumerated and
per-analyst layers too, and a layer nothing helps is reported at the build rather than refused at
the request ([decision 0092](decisions/0092-the-build-reports-a-layers-shape-and-no-layer-carries-a-declared-bound.md)).

**Filtering before evaluation is refused and need not be argued again.** Anything that skips
evaluation on geometry is a disclosure decision taken on a stamp — the shape
[decision 0041](decisions/0041-pins-become-a-staleness-stamp.md) already refused for pins — and the
measurement removes its motive: the route that would have justified it is three orders of magnitude
the wrong side of the one needing no such filter.

The honest third shape, for whoever finds the invalidation rule hard: derive per request by
*projecting* rather than crossing, which is the caching arm's work done per request instead of per
move. 15 ms per request at 263 artifacts, ~15× the cached cost. It loses whenever a generation
serves more than one request.

What the measurement does **not** cover, stated rather than glossed: a **spatially clustered**
predicate — a boundary whose members share a region — where the crossing's domain is small and the
projection's cost is unchanged. That case favours the crossing and is unmeasured.

What the discussion was called for remains true, and is why it was held first: a predicate
membership is answered by a masked scan per artifact and **the cut cannot save it**, because the cut
runs after the verdicts and so serves fewer artifacts while never evaluating fewer. The budget looks
like a cost control there and is not one.

Also still open at Stage 6 and unchanged: ⊘ the proportional criterion's denominator, since *"the
points inside this shape"* declares no member set and its size moves at every write.

### 1.2 The conformance suite — found red, now green (done)

Found while writing the I3 row and **fixed** on branch `conformance/fixture-drift`
([`design/conformance.md`](design/conformance.md) r14 diagnosed it, r15 is the fix). 432 of 432
pass. Nothing in the engine moved: the diff is eleven Python files, every one of them the fixture
learning something about the build it had been assuming.

Two things are worth carrying out of it.

**The suite could not spawn a server at all** — `tessera serve` has taken `--deployment` in place of
`-c` since the configuration rework, and `oracle/harness.py` still passed the old spelling — so no
module had run since that landed, CI included. A suite that is not in `CLAUDE.md`'s gate can die
silently for weeks.

**A check that compares sets cannot see a permutation.** The mask catalogue is designed so that
`entity_id == source_id`, and
[decision 0073](decisions/0073-entity-ties-are-ordered-by-morton-code.md)'s Morton tiebreak ended
that. `verify()`'s block check compared each block's postings against its entity range **as a set**,
which a within-block permutation preserves exactly — so the check whose comment said it re-derived
the identity had never tested it, and every per-item join in the suite was silently comparing one
item's planted value against another's. It now compares the two spaces item by item.

One fixture-shape fact came with it and no bridge fixes it: **an entity id's position inside its
block now tracks its position on the map**, so a test denying "the lowest 2,400 entities" was
denying a contiguous region.

## 2. What was built on 2026-08-20, so you do not rediscover it

**The dependent drop.** `serve_artifacts` collects a `Placement` per served artifact — its own
address, its parent's, and the address of what it depends on — and `orphaned_dependents` removes any
dependent whose target's layer is in **this** request and whose target is not in the response. It
runs **before** the parents resolve, so a dropped dependent takes its own name out of `served_at`
and cannot be named as anything's parent. Chains cascade through a worklist over the edges the
response holds. The attachment identifier never reaches the wire, and that is the ruling: handing a
client the identifier names an artifact the response does not contain, which is `parent_id`'s null
rule at the other grain.

**The trap is tested, and it is what stops a naive fix.** A request naming the dependent layer alone
— "give me just the labels" — finds no target and a naive lookup drops every label. That is a
legitimate call, refusing it is outside the disclosure surface, and
`a_request_for_the_labels_alone_keeps_every_label_it_would_have_had` is the guard. Deleting the drop
turns the other two red and leaves that one green, which is what it is for.

**A target outside the viewport is dropped by the same rule.** Rare by construction — a label's
members are the documents it was drawn from — and separating it from the cut case would mean
carrying a reason per absent candidate through a pass that deliberately collapses reasons. Recorded
because it is the one behaviour here that is not the budget.

**The I3 fixture is built backwards from the property's edge**, and that is the whole of why it is
not the mask catalogue's. `oracle/label_fixture.py` plants one entity carrying a term one principal
holds and the other does not, inside the widest generating set and nowhere else — so "one member
short" is a fact about the corpus rather than a hope, and it is asserted from the masked counts
before anything rests on it. Three artifacts of one layer over one membership differ only in which
generating sets their contents were drawn from, so **the same response carries an absence and its
control**: without the control, deleting containment altogether would still leave the narrower
principal seeing nothing and the test reading green.

**The pin is not re-presented in the cache half**, and §4.4 asked for it. Decision 0041 made a
geometry stamp advisory and never authorisation, so presenting one is an ordinary request with an
ordinary answer and could not hold a suppression out either way. What carries session state across
an overlay change is the token, and that is what the test holds fixed.

## 3. What Stage 5 built, so you do not rediscover it either

**The hierarchy is in the edges, and only the parent direction is durable.** Children are derived by
inverting parent edges per level at serve time, so no deletion has to keep two copies of one fact
agreeing. `children_keys` was deleted in the configuration rework for exactly this reason.

**A layer's edges run either within a level or between them, and may not mix**
([decision 0087](decisions/0087-cross-level-edges-are-information-not-rollup.md)). `nested` is the
clustering case; **`tiered`** is the levelled case, the declaration value that had been missing and
the reason the third shape could not be expressed at all. It is now load-bearing beyond Stage 5:
the artifacts-from-points list-column reader dispatches on it, one entry per level under `stacked`
and `tiered`, a lineage under `nested`.

**A budget takes nothing on a tiered layer**, exactly as on a flat one. There is no depth to trade,
because the resolution is the client choosing a level. Substituting a state for its counties is not
the honest coarsening that substituting a parent cluster for its children is.

**The cut climbs to a *passing* ancestor, never to a depth.** An integration test caught the
alternative: a suppressed root blanked its children's regions entirely at a budget of one. If you
touch `cut.rs`, that property is what the differential test against the reference implementation
over 200 random forests is protecting.

**The proportional gap is real and easy to test vacuously.** Under an absolute member requirement
(`require_member_visibility = { count = n }`) a passing child never sits beneath a failing parent;
under a proportional one (`{ fraction = p }`) one does. The first version of that test
passed for the wrong reason — the parent was covered by the frontier rather than failing its bar.
If you write one, publish the parent into a second layer where nothing covers it.

**`parent_id` is on the artifacts frame and its null rule is the disclosure rule** (leak register
**C29**, contracts §3.2). Null means *no parent in this response*, covering both a root and a parent
that exists and was withheld. Do not model a "hidden parent" state; there is nothing to fill it
from. The TypeScript client and the viewer both read it this way, and the viewer's `servedLineage`
treats an unresolved link as no link.

**⊘ Per-branch depth stays unspecified.** A budget resolving to different depths in different
branches is the honest general case; the agreement property two budgets rest on is written for the
single-depth form, so do not add it casually.

## 4. Things that will bite

**The demo corpus is the fixture, and it is re-derivable.**
[`test_corpora/arxiv/`](../test_corpora/arxiv/README.md) writes the whole corpus and its one
declaration; `run_demo.sh --scale notebook` builds it, serves it and opens the viewer. Use it
before reasoning about behaviour from the types — three of Stage 5's
findings came from running it and none from reading.

**The view's extent is `auto`, and that is load-bearing.** The rung writes raw UMAP coordinates
and the frame fits a box around exactly those numbers. Hand-scaling them against a stated extent is
what this notebook used to do, and it put the entire corpus in a forty-cell speck in one corner of a
65 536-cell world — silently, with no clamp and no error, and it survived a full review because
every count was still correct. Do not reintroduce a scale factor anywhere.

**`--carry-id-key-from` does not carry the term dictionary.** Term ids are assigned by first
appearance, so a new term appearing earlier renumbers the dictionary, changes the signature sort,
changes permanent entity ids, and changes every `tessera_id`. **Do not rebuild a bundle you intend
to keep identities across.** §1.2 is that warning arriving from the inside rather than from a
rebuild, over a fixture that assumed the numbering would hold.

**⊘ The artifact's own-terms gate is unbuilt.** No per-artifact term is stored, and a layer
declaring `artifact_visibility = { field = … }` withholds — fail-closed. Comments in the serving
path still point at "Stage 3" as where it arrives; the stage pointer is stale, the gap is real.

**`tessera-engine --test write`** has timing-sensitive deny-latency cases that can fail under load
from a concurrent cargo invocation. Re-run that binary in isolation before reporting one as yours.

**Work in a worktree** — `.claude/worktrees/<name>` on its own branch
([`agents/parallel-work.md`](agents/parallel-work.md)), whether or not anything runs beside you.
Stage 5 and the configuration rework shared a branch and a working tree for a day, and the cost was
paid in unpicking each other's staged files, not in the code.

## 5. The gate

```bash
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-layers.sh
bash scripts/check-clients.sh
python3 scripts/check-doc-links.py
python3 scripts/check-corpus-integrity.py
```

Baseline **1819 Rust tests, 0 failing, 11 ignored**, plus **216 client tests** (5 + 152 + 59, of which 6 are skipped live-service cases).

**`--no-fail-fast`, and read the count.** Without it cargo stops at the first failing binary and
skips the rest, so a run reporting no failures beside a *smaller* passing total reads as success.
That has been mistaken for a green gate here.

**The conformance suite is not in this gate**, and it is worth running anyway (§1.2):
`python3 -m pytest conformance/tests -q` — 432 pass, of which
`conformance/tests/test_label_containment.py` is 16. It runs in CI, where it had been failing since
the configuration rework without that being visible here. ⊘ `conformance/suite` needs Python 3.11+
for `tomllib` and does not run on this machine.

## 6. Where authority lives

| | |
|---|---|
| [`artifact-delivery.md`](artifact-delivery.md) | **the status record** for artifact work, by owner direction — not GitHub issues. Move it with the work |
| [`design/configuration.md`](design/configuration.md) | the normative declaration surface: the closed key set, every refusal, the worked example |
| [`design/artifacts-from-points.md`](design/artifacts-from-points.md) | artifacts declared by their points — readers, `value_set`, lineage, growth, the wire column, minting, and §8's open items |
| [`design/annotation-write-cycle.md`](design/annotation-write-cycle.md) | artifact-side write semantics (§6.1); §3.4 is the timing table |
| [`design/annotation-representation.md`](design/annotation-representation.md) | the representation, and §5.0.4 on edges constraining write order |
| [`design/conformance.md`](design/conformance.md) | the invariant matrix (§4.6) and, at r14, the suite's own state |
| `decisions/0080`, `0082`, `0083`, `0087` | the per-artifact test; hierarchy in edges; the request-time budget; the two edge shapes and their uses |
| `decisions/0088`–`0091` | the two visibility axes; the dependency edge; a vocabulary's single axis; build is ingest |
| [`artifact-config-handover.md`](artifact-config-handover.md) | the configuration rework's own map — renames, the input shape, and its open items |

**Refuse only where something leaks or is irreversible.** Both recent bodies of work produced
refusals that had to be unpicked — a `public` label under a gated parent, a list column on a `flat`
layer, a self-parent check that rejected a legitimate taxonomy where an archive has no subclass.
Each looked principled and each foreclosed something a caller legitimately wanted. Outside the
disclosure surface, report the numbers and let the operator decide.

**Delete this document when its work list is empty.**
