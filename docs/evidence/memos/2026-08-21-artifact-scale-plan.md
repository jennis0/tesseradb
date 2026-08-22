# Implementation plan — the artifact scale campaign

**Date:** 2026-08-21 · **Status:** Plan — evidence, not normative. The design it executes is
[`artifact-serving-at-scale.md`](../../design/artifact-serving-at-scale.md), now ruled by
[decisions 0092–0094](../../decisions/README.md), with the selection surface designed in
[`2026-08-21-artifact-layout-selection.md`](2026-08-21-artifact-layout-selection.md). The status
record is [`artifact-delivery.md`](../../artifact-delivery.md), restaged in the same change that
lands this memo; where the two disagree, the delivery record wins.

**Where it has got to (2026-08-22):** tracks 1–4 are landed and merged (the review dispositioned,
the cadence fixes, the interleaving battery, the generator's arm), the integrated gate reads 1 855
passed / 0 failed / 11 ignored, and track 7's corrected re-measurement is complete — the design
memo's §7 carries the measured tables, with two negative results (the 10⁹/10⁷ cell on 47 GB; the
blocks = 8 anomaly) and the expression census recorded. What remains is the serving-path build
(track 5), the spatial route (6), and the campaign proper (8–9).

## What this is

The scale investigation ended with a measured design and three owner rulings. This memo turns it
into work: **the serving path's structures**, **the layout selection surface**, **validation at
scale in a realistic scenario**, and **artifact writes concurrent with point writes**, tested across
the configuration matrix.

Every track ends green on the full gate and is one commit in its own worktree
(`.claude/worktrees/<name>`, per [`agents/parallel-work.md`](../../agents/parallel-work.md) and the
delivery record's convention). Pre-release rules apply
([decision 0048](../../decisions/0048-no-deployments-exist-so-delete-rather-than-support.md)): a
changed format means recreated artifacts, with version bumps as fail-closed guards only, and no
compatibility shims at any point.

**The target scenario** (owner-set): one shared bundle at **10⁹ points**, **10⁶ artifacts primary
with 10⁷ as targeted probes**, **1M+ unique access terms**, and **many concurrent principals each
with their own `M_auth`** — the mask that fixes what one viewer may see. Cross-session mask sharing
is out of scope; principals do not share masks. Concurrency is validated as a **sweep** — 1, 8, 32
and 128 sessions at mixed breadths — **reporting the envelope**, not a pinned pass/fail count.
Validation runs on this machine (47 GiB, 6c/12t under WSL2); anything exceeding it is **stated as
modelled**, never quoted as measured.

## The rulings this rests on

Three were taken today and each removed work this plan would otherwise carry.

- **[0092](../../decisions/0092-the-build-reports-a-layers-shape-and-no-layer-carries-a-declared-bound.md)
  — the build reports a layer's shape, and no layer carries a declared bound.** The scale memo's §9
  question is answered (c) always, (a) wherever the layer partitions, and no bound at all. **This
  cancels a whole stage of the earlier plan**: the per-request refusal on a layer's declared artifact
  count, which had been an owed item since the model was written, is withdrawn rather than deferred.
- **[0093](../../decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md) —
  nothing is materialised per token over the artifact population.** Containment comes from a
  build-time partition over terms. This deletes the per-session counting cache the earlier plan
  budgeted a measurement and an owner ruling for.
- **[0094](../../decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)
  — the serving layout is automatic, overridable per layer, and re-evaluated at every fold.** This
  is what the earlier plan called route selection, at a grain the row-major layouts changed.

Two further owner directions from the same session shape the order rather than the content.
**Scale validation is pulled forward** ahead of runtime artifacts — the restage the delivery record
now carries. And **the adversarial review runs now**, over the ruled memo *plus* the selection
surface, before any of it reaches the serving path.

## What changed since the earlier plan

Recorded because a reader who saw the route-selection plan will otherwise look for pieces that are
gone.

| earlier plan | now |
|---|---|
| Stage 2 — the per-request bound, first code commit | **deleted** (0092) |
| Stage 4/5 — histogram machinery, then route selection | **the serving-path build** — the memo's containment partition (§4.2), hierarchical index and extents (§4.4), and row-major layouts (§5), with 0094's selection surface over them |
| Stage 7 — the per-session counting-pass cache, and the ruling it wanted | **deleted** (0093). What remains are the memo's §7.4 reductions and its two ordered measurements |
| Stages 3, 8, 9, 10 | **stand as written** — interleavings, generator fixtures, the campaign, the configuration matrix |

**The column-histogram route and the row-major label layout are one structure.** Stage 6's cost
discussion measured a masked histogram over the `attrs/` value column at 175 ms against 462 for the
per-artifact loop; the scale memo's §5 is that structure in row space rather than entity space —
which is where ~120 ms of the 175 went — generalised to a list per row so it covers overlapping
layers too. They are not two routes to choose between.

## The tracks

| # | Track | Branch | State | Needs |
|---|---|---|---|---|
| 0 | Adoption — the rulings, this plan, the restage | `artifacts/scale` | **this change** | — |
| 1 | The adversarial review | its own session | **running** | 0 |
| 2 | Cache cadence — the two live defects | `artifacts/cache-cadence` | **in flight** | — |
| 3 | Write-path interleaving battery | `artifacts/interleavings` | **in flight** | — |
| 4 | The generator's artifact arm reaches disk | `artifacts/generator` | **in flight** | — |
| 5 | The serving-path build | — | not started | 1, 2 |
| 6 | The spatial predicate route | — | not started | 5 |
| 7 | Reductions and the ordered measurements | — | not started | 2 for one of them, 5 for the rest |
| 8 | The scale campaign | — | not started | 4, 5, 6, 7 |
| 9 | Configuration matrix, census, conformance | — | partly startable now | 5 for its predicate cells |

Tracks 2, 3 and 4 are independent of each other and of the review, which is why they were dispatched
first. Track 3 must land **before** track 5 moves the write paths.

### 1 — The adversarial review

One review, two or three lenses, in a session of its own, over
[`artifact-serving-at-scale.md`](../../design/artifact-serving-at-scale.md) as ruled and
[`2026-08-21-artifact-layout-selection.md`](2026-08-21-artifact-layout-selection.md). Findings are
dispositioned in a single pass; re-review only if a disposition changed the design's shape.

**What it must attack**, beyond whatever it finds: that geometry is used as a candidate generator
and never as an answer (§4.1); that the containment partition's two unmodelled cases —
suppressions, and generating sets that lost members in projection — are corrections and not holes;
that a layout flip is invisible to every client; and the five questions the selection memo's §8
names against itself.

### 2 — Cache cadence *(landed 2026-08-21, merged)*

The two live defects in the memo's §8, neither about scale and both fatal to everything above them.

- **§8.1 — the store version is global.** One artifact write invalidates every cached row form in
  every view: 138 s at 10⁷ artifacts, 16 s at 10⁶. Under read-write load the cache never survives to
  be used. It needs to be per (layer, level), and ideally patched rather than rebuilt — a write
  touches a handful of ordinals.
- **§8.2 — the lineage is rebuilt per request**, per level, under the artifacts lock, from something
  that depends on neither the mask nor the viewport. It is 87 ms at 10⁷ because it now carries
  depth, which is the right home for depth and the wrong cadence for the object. Held per
  generation, it is the difference between the cold and warm columns of the cut's table, and ~96 ms
  off every cell of the §7.2 grid.

**Must prove:** a write to one layer leaves another layer's row forms intact; a second request in
one generation does not rebuild the lineage; the cut's warm column is what a request actually pays.

### 3 — The write-path interleaving battery *(landed 2026-08-21, merged)*

A new test file. **Zero tests construct these interleavings today**, and track 5 moves the write
paths. The mid-window arm applies immediately, so deterministic orderings need in-process sequencing
rather than threads.

Eight cases: a publication landing between a batch's admission and its close; two batches in one
window naming the same unminted key; a growth racing window close in both orders; a fold racing a
growth, with crash-replay equivalence on both sides; a suppression racing a publish, and mid-fold;
window atomicity around the single fsync, so rows and artifact joins commit together or not at all;
one threaded stress case with a concurrent reader asserting monotone freshness; and the build-entry
case — a build's artifact publish riding the same window as point admission
([decision 0091](../../decisions/0091-build-is-ingest-into-an-empty-database.md)).

**Must prove:** every ordering has one correct outcome and the code produces it; nothing here
depends on wall-clock timing to reproduce.

### 4 — The generator's artifact arm reaches disk *(landed 2026-08-21, merged)*

Generator-first rather than replica-carried, for this campaign only: closed form in both directions
buys census verification at sizes where no expectation can be stored. Replica-carried artifact tiers
move to the filters-and-search stage, where real geometry earns them.

- A **partition arm** — single-valued, closed-form both ways. The existing interval and scatter arms
  overlap, so neither can feed an attribute predicate.
- **Attribute-column emission** into the points output and the declaration; **members-file
  emission** for enumerated twins; **spatial boundary layers** at depth-*d* tiles, closed form.
- An **artifact census verb** — expected masked counts per (grant, artifact) in closed form. This is
  what replaces the enumerated twin at 10⁹, where a twin's member file is itself ~10⁹ rows; twin
  equality still runs at the 10⁷ and 2.5×10⁸ tiers.
- **`TERM_SPACE` parameterised** (default unchanged at 1024) so the campaign bundle carries ~10⁶
  terms *and* 10⁶ artifacts with the census still closed-form. The scaled corpus's surnames tier is
  the real-data cross-check, and mask-build flatness is already measured at 117M terms.

**Must prove:** the census and the twin agree exactly at a tier where both exist, which is what
licenses using the census alone above it.

⊘ **One decision is owed at execution**: whether to spend the overnight 10⁹ generator build and its
disk, or to headline at 2.5×10⁸ with one 10⁹ confirmation.

### 5 — The serving-path build

The heart of it. Everything the scale memo measures is probe-side today — the control flow lives in
`crates/tessera-bench/src/bin/artifact_serving_scale.rs`, not in the engine. Only the cut is built.

In this order, because each makes the next cheaper to measure:

1. **The containment partition** (§4.2) — one byte per artifact naming an interned boolean
   expression over terms, plus a bitmap per distinct expression, with the suppression intersection
   and the lost-in-projection fold that the probe does not model.
2. **The hierarchical row-range index and the per-artifact extents** (§4.4). All three of that
   section's requirements are load-bearing and each was found by a measurement that failed without
   it: a hierarchy rather than a fixed granularity, the extent beside the tree, and an
   allocation-free walk.
3. **The row-major layouts** (§5) — a label per row where the layer partitions, a list per row where
   it overlaps.
4. **The selection surface** — the layout enum, the per-(layer, level) record, the `layout` override
   key with its refusal set, and the fold's re-evaluation, per 0094 and the selection memo as the
   review leaves it.

**Must prove:** the served set is identical to today's, ordinal for ordinal, at every mask and every
zoom, and count for count on the row-major route — the probe's assertion becomes a unit invariant.
Then the **enumerated-twin equality**: one layer built by rule and by list returns identical masked
counts for every principal and every viewport, on both layouts, on both sides of a fold and on both
sides of a forced override flip. And **ingest freshness**: a point ingested with an attribute value
counts on the next request with nothing rebuilt.

### 6 — The spatial predicate route

Build-time decomposition of the declared shape to Morton ranges at a declared depth — bbox tiles
filtered by polygon-intersects-tile. **The ranges are the membership**, which makes
ingest-freshness true by construction rather than by a refresh. Counting is `count_range` per
artifact; candidacy is range-against-tile arithmetic.

⊘ Still open at this stage and unchanged by anything today: the **proportional criterion's
denominator** for a predicate layer, since *"the points inside this shape"* declares no member set
and its size moves at every write. Until it is ruled, a predicate layer may declare an absolute
criterion or none.

### 7 — Reductions and the ordered measurements

**Two measurements first**, because both change what is worth building:

- **Re-run the §7.2 grid on the `nested` arm at 10⁷.** §7.2 is measured on a fixture whose tree is
  unrelated to its geometry — which is not a hierarchy, and which made the cut look like the bound
  when it is 0.3–11.4 ms on a real one. Every treed figure at a partial viewport in that section is
  suspect until this runs.
- **Price per-signature counts against a real corpus's signature distribution**, before building
  them. They are the hierarchy's dominant term and ~1.3 MB if stored only for the coarse nodes — but
  the storage scales with the **signature** count rather than the artifact count, and a thousand
  distinct signatures stored per node would be 40 GB at 10⁷.

**Then the three reductions the memo scopes** (§7.4), in the order they are worth taking:
per-signature counts for coarse nodes, if the measurement above says yes; scratch buffers for the
cut's sweep, whose floor is page faults rather than work (769 475 minor faults across the probe) and
which trades a pure function for reused state, so it wants a ruling; and **handing the cut a bitmap**
rather than a materialised slice, which also removes the ~240 MB of `passing` tuples a wide request
allocates and which appears in no figure anywhere.

⊘ **Two things are measured and reverted**, recorded so they are not re-attempted: pre-sizing the
cut's depth buckets from a counting pass, and `ArtifactRows::intersects`' early exit at whole-map
zoom.

### 8 — The scale campaign

Probe directories under `probes/`, collated per convention, with a closing memo here.

**The matrix**, per tier (10⁷ / 2.5×10⁸ / 10⁹): artifact counts 10⁴ / 10⁶ / 10⁷; layouts
artifact-major / row-major / spatial, plus forced-override arms on both sides of each crossover;
principal breadths broad / median / narrow; concurrent sessions 1 / 8 / 32 / 128 at median breadth,
broad capped at 8. **Serving during a fold** at 10⁹ with 32 live sessions is the named gap — the
fold's artifact pass is measured unloaded only. **Ingest during serving** for freshness, and a
**layout flip observed by live sessions**.

Each run records p50/p99 per phase — counting, candidacy, verdict, cut — plus RSS, page-cache delta,
cache hits, fold duration, the layout decisions taken, and first-viewport cold against warm.

**Acceptance**, tied to existing claims and gated relatively after the first baseline run:

- The §7.2 grid holds at the target — 10⁷ artifacts over 10⁹ points, the whole grid of principals
  and zooms inside the campaign's budget, with both axes swept through their middles rather than
  sampled at their extremes.
- The predicate arm's two figures stand within 20%: ~175 ms whole-map for the column route and
  ~462 ms for the per-artifact loop, at 10⁶ artifacts over 10⁷ rows.
- Crossovers land within 2× of the selection surface's table.
- The fold under load is within 2× of the measured 32.8 s unloaded.
- Point serving is unregressed — 9.9 s cold, 285 ms warm at 10⁹.
- **10⁹ row-major counts match the generator census exactly.**

**Two cautions this campaign has already paid for**, both in the probe's fixture section. *The
fixture is the experiment* — six corrections were needed and every one moved a headline further than
any option did. And *a grid whose extremes are cheap says nothing about its interior* — sampling
four viewports and three coverages understated the worst request by 2.1×.

### 9 — Configuration matrix, census, conformance

- **Build refusals and coverage**: the predicate-at-build stored-membership refusal, which nothing
  reaches today; a build of an attribute-predicate layer with vocabulary minting; `stacked` at
  ingest.
- **Census extension**: parametrise entry point (build / control publish / ingest-mint) × membership
  kind (enumerated / attribute / spatial) × shape (flat / nested / stacked / tiered) × the
  criterion and own-terms cells — small *N* per cell, closed-form oracle, at the same four
  checkpoints the census already uses: before a write, after deletions, after the fold, after a
  restart.
- **Conformance, minimal scope**: add `layers` to the battery's Viewport query with one enumerated
  and one attribute-predicate layer, and regenerate recordings. **No recording carries an artifacts
  frame today**, so this is the only cross-stage regression net the artifact channel has.

## Verification

Per track, and read the counts:

```bash
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-layers.sh
bash scripts/check-clients.sh
python3 scripts/check-doc-links.py
```

Conformance runs in CI — `python3 -m pytest conformance/tests -q`, 432 the baseline. Track 5's
twin-equality and track 8's census exactness are the two ends of the correctness net; the campaign's
numbers land in `probes/` and are gated relatively thereafter.

**Measurement runs do not share the machine with compile loops.** The deny-latency tests are
load-sensitive, and three tracks are in flight.

## Out of scope, stated

Cross-session mask sharing and the shared caches it would allow; runtime artifacts and the edit
design pass; membership-as-filter, search, the `excluding` complement's price and replica-carried
artifact tiers — all of which are the delivery record's Stage 9; multi-view and multi-partition
serving; and the FST dictionary, which is not needed at 10⁶ terms.
