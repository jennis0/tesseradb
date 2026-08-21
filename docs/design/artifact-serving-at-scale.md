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

#### 3.3 A different layout for artifacts that are everywhere *(new, and it is an inversion)*

§3.1 helps a layer whose artifacts are *somewhere*. For one whose artifacts are everywhere, no tree
helps — but a different **layout** does, and it exploits a property the scattered shapes have and
clusters do not: **a single-valued attribute predicate partitions the corpus.** Every point carries
exactly one value, so the memberships are disjoint and cover the row space, and the natural storage
is not a set of per-artifact bitmaps at all. It is one label per row.

That inverts both questions from artifact-major to row-major, and both then cost **points rather
than artifacts**:

| | artifact-major (today) | row-major |
|---|---|---|
| candidacy | `intersects` per artifact | one scan of `viewport ∩ M_auth`, marking labels — **every artifact at once** |
| the count | `masked_count` per artifact | one histogram over `M_auth` — no viewport, so it lives in §3.2 |

**Row-addressed, and that is the whole of the change.** `attrs/` already holds this column, but
addressed by **entity**, which is right for a filter and wrong for a viewport: a viewport is a set
of contiguous *row* ranges, so reaching the entity form costs an `entity_of` per row. That is
visible in Stage 6's own measurement — ~120 ms of its 175 ms was the inversion rather than the
counting. A row-addressed copy removes it, and it is a row-space projection of a durable entity-space
column, which is the same lifecycle `ArtifactRows` already has.

**Not a new mechanism.** `artifacts-from-points` already *reads* this layout — an integer key column,
or a list column naming the artifacts a point belongs to — and converts it into artifact-major
bitmaps on the way in. The option is to keep what the build was handed.

**Measured**, over 10⁸ points, single-threaded, both routes against the same per-token structure.
The point of the table is the pair of **columns**, not the rows: one is flat in the artifact count
and the other is not.

| viewport | artifacts | shipped | §3.1 artifact-major | **row-major** |
|---|---:|---:|---:|---:|
| whole map | 10³ | 376 ms | **1.9 ms** | 333 ms |
| whole map | 10⁴ | 2 320 ms | **15.5 ms** | 332 ms |
| 6.25% | 10³ | 102 ms | 26.1 ms | **23.4 ms** |
| 6.25% | 10⁴ | 448 ms | 193 ms | **23.3 ms** |
| 0.39% | 10³ | 79.0 ms | 2.06 ms | **1.48 ms** |
| 0.39% | 10⁴ | 271 ms | 21.2 ms | **1.56 ms** |
| 0.024% | 10⁴ | 259 ms | 5.07 ms | **0.18 ms** |

**Ten times the artifacts, and the row-major route does not move** — 23.4 → 23.3 ms, 1.48 → 1.56 ms
— while the artifact-major one goes up sevenfold to tenfold. Its cost is ~4–5 ns per visible row in
the viewport and nothing else, which puts a 6.25% viewport over 10⁹ points at ⊘ ~280 ms *(modelled
from that constant)* however many artifacts the layer holds.

The candidate sets and the masked counts are asserted identical to the shipped loop's — ordinal for
ordinal and count for count, every artifact, before either route is timed.

**The two layouts cross, and they cross where you would want them to.** Row-major scans the
viewport, so it is cheapest when the viewport is small and dearest at whole-map zoom where it walks
the corpus; artifact-major is exactly the reverse, because at whole map `membership ⊆ viewport`
holds for every artifact and §3.1's extent test answers the whole layer from the per-token structure
without scanning anything. So the rule is *take the cheaper*, and at 10⁸ points the crossover at
whole-map zoom is around **2×10⁵ artifacts** — below it the extent test wins, above it the flat scan
does. Both are exact; neither is an approximation of the other.

### The argument that is not about speed

For a scattered layer at the target, the artifact-major form **does not fit in memory**, and the row
major one is flat. From the residency campaign's measured constant of 78.5 B per Roaring container
on scattered membership, over 10⁹ rows:

| scattered artifacts | members each | artifact-major | row-major |
|---:|---:|---:|---:|
| 10⁴ | 10⁵ | 12.0 GB | **4.0 GB** |
| 10⁵ | 10⁴ | 78.5 GB | **4.0 GB** |
| 10⁶ | 10³ | 78.5 GB | **4.0 GB** |

⊘ **Derived, not measured at 10⁹** — the per-container constant is measured and the arithmetic is
this table. It agrees with what the residency campaign observed directly: its scattered arm at 10⁷
artifacts was **OOM-killed** rather than merely slow. The row-major form is one `u32` per row
regardless of how many artifacts there are — narrower still at the `u8`/`u16` widths
`configuration.md` §1 already declares for a category — and it is a mappable array rather than
anonymous allocation, so it costs page cache the kernel can reclaim rather than heap it cannot.

⊘ **Single-valued only.** An overlapping or multi-valued layer needs a list per row rather than a
label. That is the same inversion at a larger constant, and it is the shape `artifacts-from-points`
already calls a list column — but it is not measured here.

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

- **(a) Store it row-major — §3.3, and it is now measured rather than argued.** A single-valued
  predicate partitions, so one label per row answers every artifact at once. Flat in the artifact
  count (23.4 ms at 10³ and 23.3 ms at 10⁴, same viewport), and at the target it is the only layout
  that **fits**: 4 GB against 78.5 GB. This removes the wall for the attribute-predicate case
  outright rather than bounding it.
- **(b) Enumerated scattered sets are small, and could be required to be.** A per-analyst selection
  or a terms-as-artifacts layer is made by a person or by a vocabulary, not by a clusterer; the
  realistic counts are thousands. At 10³–10⁴ the wall is 1.5–15 ms. **This is the ruling worth
  making**: whether a layer with no column and no locality carries a declared bound, refused or
  warned at config time.
- **(c) Accept the cost and let the operator see it.** Report the per-layer shape in the build's
  frame report — blocks per artifact is one number and the build already computes the row form it
  comes from — so a layer that will be slow says so when it is built rather than when it is panned.

Recommended reading of the house rule: **(c) always, plus (a) wherever the layer partitions, and (b)
as a warning rather than a refusal.** Nothing here leaks and nothing here is irreversible, so a
wrong guess costs a rebuild.

**With (a) in place the wall moves off the shapes that would actually hit it.** What is left
un-helped is a layer that is scattered *and* overlapping *and* numerous — an enumerated set that
does not partition and has no column. Every real instance of that shape is human-made or
vocabulary-made, which is why (b) is a warning about a case rather than a bound on the design; and
the list-per-row form in §3.3 is the same inversion again if one ever turns up.

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

**5.4 What the request allocates after the verdict, which is the next thing anyone would hit.**
The serving loop collects `passing: Vec<(u32, EntityId, u64, Option<u32>)>` — every artifact that
cleared the predicate — and hands its ordinals to the cut. At 10⁷ passing that is **~240 MB
allocated and freed per request**, on top of the verdict this memo is about, and it is not in any
figure here: the probe measures the predicate and stops.

It is not the same problem, and it has a different answer. The routes in §3 leave the passing set as
a **bitmap**, so the materialisation is a choice rather than a consequence — and what actually has
to be materialised is bounded by `artifact_budget`, not by the population. The cut is what stands in
the way: it takes `&[u32]` and computes a frontier over the whole set. Whether it can take a bitmap
instead is an ordinary question about `cut.rs` and is not a disclosure one — the cut is a rendering
choice ([decision 0083](../decisions/0083-the-frontier-is-a-request-time-budget.md)) and every
artifact in either set already cleared its own test.

⊘ **Unmeasured.** Named here because a reader who takes §3 and stops would find it, and because it
is the reason the 131 ms figure is a *verdict* cost and not a *response* cost.

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
