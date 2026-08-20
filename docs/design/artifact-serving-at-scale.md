# Serving ten million artifacts — technical options

**Date:** 2026-08-21
**Status:** Options memo, **for owner decision**. Nothing here is built and nothing here is ruled.
Every figure is measured on the shipped structures by
[`probes/2026-08-20-artifact-serving-scale/`](../../probes/2026-08-20-artifact-serving-scale/README.md);
where a claim is modelled rather than measured it says so at the claim. Extends
[`annotations.md`](annotations.md) and [`annotation-representation.md`](annotation-representation.md),
which stay normative for the model — **no option here changes what an artifact is, what is served,
or what a client sees.**

## 1. The target, and whether it is met

**10⁷ artifacts over 10⁹ points, inside a second, on one core.** One core because a serving node
carries ten or more simultaneous viewers, so a budget met by spending the machine is not met.

**Measured, on the clustered arm at 10⁷ artifacts, single-threaded, worst-case request
(a principal who sees everything, at whole-map zoom):**

| viewport | shipped | with §3's structures | |
|---|---:|---:|---:|
| whole map | 2 875 ms | **131 ms** | 22× |
| 6.25% of the map | 819 ms | **9.8 ms** | 84× |
| 0.39% | 679 ms | **1.05 ms** | 650× |
| 0.024% | 672 ms | **0.29 ms** | 2 285× |

And the same request shapes for principals who see less of the corpus, where the gap widens because
the per-token pass has already removed most of the population:

| principal sees | whole map, shipped | whole map, with §3 | |
|---|---:|---:|---:|
| everything | 2 875 ms | **131 ms** | 22× |
| 9.4% | 2 712 ms | **62.7 ms** | 43× |
| 3.1% | 2 364 ms | **25.5 ms** | 93× |

So the answer is **yes for artifacts that are somewhere, and conditionally for artifacts that are
everywhere** — which is §4, and is the part that needs a ruling rather than an implementation.

## 2. Where the time goes

The request path is `O(artifacts)` four separate times, and **three of the four have no request in
them**:

| pass | what it is | depends on |
|---|---|---|
| 1. candidacy | `intersects(ordinal, tile_rows, mask)` for every ordinal | mask **and viewport** |
| 2. the number | `masked_count(ordinal, mask)` — served, and the criterion's input | mask only |
| 3. containment | `satisfied_rank(ordinal, mask, …)` — `\|G ∩ M\| == \|G\|` | mask only |
| 4. lineage | `Lineage::new(store.level(…))` — every parent pointer, per request, per level | **neither** |

Two properties of the shipped shape are worth stating before any option:

**Cost tracks containers touched on both sides, and the mask side can dominate.** At 10⁶ artifacts
of a hundred members each, the shipped path gets *slower* as the principal narrows — 479 ms at a
full mask against **1 056 ms at 3.1%** — because a narrow `M_auth` is a more fragmented row-space
set and every pass walks it. The effect is not universal: at 10⁷ artifacts of ten members each the
artifact side is one container whatever the mask does, and the same sweep runs slightly *faster* as
the mask narrows. What is universal is that the shipped path never gets cheaper in proportion to
what a viewer may see. The routes below do: a narrower principal means fewer artifacts survive the
per-token pass, so there is less left per request — 22× at a full mask against 93× at 3.1%.

**The cut cannot help.** It runs after the verdicts by construction — the verdict is a per-artifact
question with no lineage input ([decision 0080](../decisions/0080-the-frontier-is-a-per-artifact-test.md))
— so `artifact_budget` bounds what is *served* and never what is *evaluated*. Nothing in this memo
changes that, and nothing needs to.

## 3. The options

### 3.0 One line, before any structure: stop allocating on the miss

`ArtifactRows::intersects` materialises `membership ∩ viewport` and then asks the mask about it —
a heap allocation per artifact per request, paid whether or not the two sets meet. At a narrow
viewport almost none of them do. `Bitmap::intersect` answers the same question without allocating,
so asking it first turns the common case into a container walk and no `malloc`.

**Measured at 10⁷ artifacts, single-threaded, on the shipped loop otherwise unchanged:**

| viewport | shipped | with the early exit | |
|---|---:|---:|---|
| whole map | 2 770 ms | 2 995 ms | 0.9× — *worse* |
| 6.25% | 884 ms | 540 ms | 1.6× |
| 0.39% | 716 ms | 310 ms | 2.3× |
| 0.024% | 689 ms | 255 ms | **2.7×** |

It loses at whole-map zoom, by about a tenth, because there every artifact *does* meet the viewport
and the extra call is pure overhead. It is worth taking anyway, and more so with §3.1 in place: the
early exit then runs only on the `open` half — the viewport's edge — which is exactly the population
where the sets usually do not meet.

**It is an early exit on a term of a conjunction, not a different test.** No artifact's verdict
moves. This is the cheapest thing in the memo and it is independent of everything else.

### 3.1 and 3.2 — the two structures that do the real work

#### 3.1 A hierarchical row-range index over artifacts *(new build-time structure)*

Rows are Morton rank, so a tile is a contiguous row range and the client's tile hierarchy **is** a
hierarchy of row ranges. Hold, per node, the artifacts whose whole membership lies inside it. A
request walks top-down: take whole subtrees where the viewport covers a node, descend only where it
cuts one. Cost is the viewport's **perimeter**, not the population.

Used two ways, and the second is where the large factors come from:

- **as a superset filter** — an artifact outside every node the viewport touches has no member
  there, so it cannot have a visible one. Skipping it withholds nothing.
- **as a containment fact** — an artifact wholly inside a covered node has
  `membership ⊆ viewport`, so *"has a visible member here"* and *"has a visible member"* are the
  same question and the second has no viewport in it. **Exact, not conservative.** On the clustered
  arm the fixture measures **1.0 row blocks per artifact**, so this is almost the whole population.

Two things it needs to actually work, both measured:

- **A hierarchy, not a granularity.** A flat index at 65 536-row blocks is worth nothing at mid-zoom
  — the viewport is then made of tiles smaller than the block, so no block is ever fully covered.
  The tile-to-block relationship moves with corpus size, so any fixed block is wrong at most scales.
- **A per-artifact extent beside it** — `(min_row, max_row)`, eight bytes. Node boundaries are powers
  of two, so an artifact at an arbitrary offset straddles one and gets promoted to the level above,
  whose node the viewport must cover sixteen times as much of. That promotion costs the `regions`
  arm its whole benefit; `viewport ⊇ [min,max]` is alignment-free and rescues it at one
  `contains_range`.

**Cost.** **4.2 MB** serialised at 10⁷ artifacts and **3.2 s** to build — beside a projection of the
same population that costs **138 s**. It is derived from the row form, so it is built where the row
form is and thrown away with it, and it is noise against what building the row form already costs.

**Disclosure.** This is the shape `MaskedSet::intersects_set` warns about, and it is **not** the use
it refuses. What is refused is serving an artifact *"wherever the box intersected the viewport"* —
unmasked geometry as **the answer**, which discloses the unmasked extent by panning. Here geometry
decides only *which question to ask*; every artifact served still cleared the masked test. The probe
asserts the served set is identical to the shipped loop's, ordinal for ordinal, at every mask and
every zoom — the assertion is in the harness, not in this paragraph.

#### 3.2 The servable-label set *(specified in `architecture.md` §8.5, unbuilt)*

Not a new idea. §8.5's cache table already carries the row —

> | Servable-label set (containment decisions) | (auth-data hash, auth-plugin version, overlay version) | overlay change |

— and its closing line already states the property: *"because containment binds to `M_auth` rather
than `M_sel`, the servable-label set is computed once per token and reused across every keystroke."*

Passes 2 and 3 have no viewport in them, so computing both when the mask is composed removes them
from the request. Two consequences that follow from §8.5's own key rather than from anything new:

- **Viewers with the same grants share one copy.** The key is content-addressed on the auth data,
  not on the session.
- **Filters cost nothing.** `MaskedSet` is deliberately blind to any attribute filter — **I12**, a
  filter may move the frontier up and never down — so a viewer typing does not invalidate it.

**Cost, once per distinct grant set: ~840 ms single-threaded at 10⁷ artifacts.** That is not
modelled — the structure computes exactly passes 2 and 3 over the whole population, and the shipped
loop's own whole-map row times those two passes over exactly that population: **596 ms + 246 ms**.
On the rayon pool the same build measures 138 ms, a 6× that says the pass is memory-bound rather
than compute-bound, which is what a walk over ten million small bitmaps should be.

Whether to spend the pool on it is a policy question and not a correctness one: it is session
establishment, not the steady-state request path, so it does not compete with the per-request budget
the way a parallel request loop would. Residency is one bitmap over ordinals plus one `u32` per
artifact — **40 MB at 10⁷**, shared by every viewer holding the same grants.

**What still runs live.** The artifact's own suppression, unconditionally, on every route: that is
`ArtifactView::verdict` step 1 and nothing may cache above it. A suppression takes effect at the
ack.

**§8.5's key is incomplete, and the gap is fail-open.** As written it is *(auth-data hash,
auth-plugin version, overlay version)*, invalidated by an overlay change — and none of those three
moves when an **artifact** does. A publication, a membership growth or a content republication
changes both quantities the structure holds: a generating set that grew has more to contain, so a
cache keyed only on the viewer would keep serving a label the viewer no longer contains. That is a
disclosure, and it is the direction that matters — growth makes containment *harder*, so the stale
answer is the permissive one.

The key needs the artifact store's version alongside the overlay's. That is not a new mechanism:
`ProjectionKey` already carries `store_version` for exactly this reason, one layer down. It is
worth writing into §8.5 whether or not any option here is taken, because the row is in the design
today and a reader would build it as specified.

**And that version must become per-layer before this is worth building** — see §5.1. A structure
invalidated by every write in the database is invalidated continuously.

## 4. Artifacts that are everywhere — the part that needs a ruling

`annotation-representation.md` §2.0 names three membership *sources*. The axis that decides cost is
a different one, and it does not line up with them: **row-space locality**.

| shape | what has it | blocks/artifact | does §3.1 help? |
|---|---|---:|---|
| clustered | HDBSCAN, point-and-radius, a level of a hierarchy | 1.0 | **yes, entirely** |
| regional | administrative boundary, spatial predicate | 1.0–1.6 | yes, and the extent is what makes it so |
| scattered | **attribute predicate, per-analyst selection, term-as-artifact** | **96.9** | **no** |

A scattered artifact touches every node, so it is never inside one and never outside one — the walk
hands the whole population back as `open`, and each one pays a masked intersection. Measured over
10⁸ points, single-threaded, at the two request shapes that bound it:

| scattered artifacts | whole map | mid-zoom (6.25%) — the worst |
|---:|---:|---:|
| 10³ | 1.6 ms | 2.1 ms |
| 10⁴ | 16.0 ms | 24.5 ms |
| 10⁵ | 159 ms | **243 ms** |
| 10⁶ | ⊘ ~1.6 s *(linear extrapolation)* | ⊘ ~2.4 s |

Linear in the artifact count, as the shape predicts. **So the wall is around 2×10⁵ scattered
artifacts at a half-second budget** — against 10⁷ clustered ones inside the same budget, which is
**fifty times** the population. Locality is the only property that buys anything at this scale, and
it buys nearly two decades of it.

Two smaller findings in that table. Whole-map is *cheaper* than mid-zoom here, which inverts the
clustered case: at whole map the viewport contains everything, so the extent test in §3.1 settles
the entire population at one `contains_range` each. And the index is a small net **cost** on a purely
scattered layer — it walks and yields nothing — so the route choice below is not only about the
request path.

Three routes exist and the choice is per layer, exactly as Stage 6's crossover already is:

- **(a) An attribute predicate has a column, and the column answers every artifact at once.** This
  is Stage 6's measured third route — one pass over the `attrs/` `ValueColumn` the predicate names,
  flat in the artifact count. Split by cadence it gets better still: the **counting** pass is over
  `M_auth` and has no viewport, so it belongs in §3.2's per-token structure; the **candidacy** pass
  is over `viewport ∩ mask`, so per request it costs what the point path already pays for the same
  viewport. ⊘ Modelled — the split is not measured, and the measured single-pass figure is 175 ms at
  10⁶ artifacts over 10⁷ points.
- **(b) Enumerated scattered sets are small, and could be required to be.** A per-analyst selection
  or a terms-as-artifacts layer is made by a person or by a vocabulary, not by a clusterer; the
  realistic counts are thousands. At 10³–10⁴ the wall is 1.5–15 ms. **This is the ruling worth
  making**: whether a layer with no column and no locality carries a declared bound, refused or
  warned at config time.
- **(c) Accept the cost and let the operator see it.** Report the per-layer shape in the build's
  frame report — blocks per artifact is one number and the build already computes the row form it
  comes from — so a layer that will be slow says so when it is built rather than when it is panned.

Recommended reading of the house rule: **(c) always, plus (a) where a column exists, and (b) as a
warning rather than a refusal.** Nothing here leaks and nothing here is irreversible, so a wrong
guess costs a rebuild.

## 5. Two defects found on the way, neither of which is about scale

**5.1 Any artifact write invalidates every cached row form, in every view.** `ArtifactStore`'s
`version` is global and `ProjectionKey` carries it, so one suppression, one grown membership or one
publication anywhere rebuilds every layer's projection everywhere. The rebuild is **138 s at 10⁷
artifacts** (measured; 16 s at 10⁶). Under any read-write load the cache never survives to be
used, and the per-token structure in §3.2 would inherit the same fate.

*What it needs:* the version to be per `(layer, level)`, and — separately — a write to patch the
projection rather than rebuild it, since a write touches a handful of ordinals. Both are ordinary
work; neither touches an invariant.

**5.2 `Lineage::new` walks every artifact in the store on every request**, per level, holding the
artifacts lock, to build something that depends on neither the mask nor the viewport. It is a
per-generation object being rebuilt per request.

**5.3 A related consequence worth naming**, since §3.2 depends on it: the overlay changes at every
deny-lane ack, and §8.5's key includes the overlay version — so a naive reading re-materialises
every token's structure on every ack. It need not. `minus` and `plus` are small, and the artifacts
they touch are exactly `index.candidates(overlay_rows)` — **the same hierarchical index answers
"which artifacts does this suppression touch"**. Patch those and leave the rest. ⊘ Modelled, not
measured.

## 6. Three things a reader will ask that the numbers already answer

**A hierarchy of levels does not multiply the cost.** The serving loop runs every level of a treed
layer, so a reader expects an *L*× multiplier. There is none: each level's cost is proportional to
its own population, and the figures above are for a **total** artifact count. 10⁷ artifacts across
six levels costs what 10⁷ in one flat level costs.

**Residency is dominated by what already exists.** The row form is 3.6 GB at 10⁷ artifacts on the
realistic arm and multiplies by view and by level ([the residency probe](../../probes/2026-08-16-membership-residency/README.md)).
Against that, §3.1's index is **4.2 MB** and §3.2's structure is **40 MB** per distinct grant set.
Neither is the memory question; the row form is, and it is unchanged by anything here.

**The measured routes are probe-side.** Every *quantity* is taken through the shipped structures —
`ArtifactRows::build`, `intersects`, `masked_count`, `satisfied_rank`, the same three-set composed
arithmetic `EffectiveMask` does, viewports from `tiles_for_bbox`. The *routes* are written in the
probe, not in `viewport.rs`. What is measured is therefore what the structures cost, not what an
integration costs; the integration is ordinary work and is not estimated here.

## 7. What is not in scope here

- **Nothing changes the client contract.** No new request field is required by any option above;
  §4(b) would add a config-time declaration, not a wire change.
- **Nothing changes the write path.** §3.1's index is derived from the row form and is built where
  the row form is built. §5.1's fix makes writes cheaper, never harder.
- **The selection functions** — rank by masked size, filter by label text — are a separate question.
  They become cheap once §3.2 exists, because the masked count is then a lookup rather than an
  intersection, but the ranking contract itself is unruled and is not proposed here.
