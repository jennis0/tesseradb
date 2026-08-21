# Serving ten million artifacts — design and options

**Date:** 2026-08-21
**Status:** **Options memo, for owner decision.** One part is built and gate-green — the cut rewrite
(§6) — because it was a data-structure choice rather than a design one. Everything else is measured
and proposed, not implemented.

Every figure comes from the shipped structures, measured by
[`probes/2026-08-20-artifact-serving-scale/`](../../probes/2026-08-20-artifact-serving-scale/README.md)
and [`artifact_cut_cost`](../../crates/tessera-bench/src/bin/artifact_cut_cost.rs); anything modelled
or derived says so at the claim. Extends [`annotations.md`](annotations.md) and
[`annotation-representation.md`](annotation-representation.md), which stay normative for the model.
**No option here changes what an artifact is, what is served, or what a client sees.**

## 1. The target, and the answer

**10⁷ artifacts over 10⁹ points, inside a second, on one core** — one core because a serving node
carries ten or more simultaneous viewers, so a budget met by spending the machine is not met at all.

**Yes for artifacts that are somewhere; for artifacts that are everywhere, only with a different
layout.** The worst request in the system — a principal who can see the whole corpus, at whole-map
zoom, on a treed layer — goes from **~1 190 ms to ~325 ms**, and every other request shape is one to
three orders of magnitude better than that.

## 2. The one idea

The request path asks four `O(artifacts)` questions and **only one of them has the request in it**.
The design separates them by cadence and gives each the layout that suits it.

| question | mask? | viewport? | belongs |
|---|---|---|---|
| does this artifact have a visible member **in view**? | yes | yes | **request** |
| `\|membership ∩ M_auth\|` — the number served, and the criterion's input | yes | no | **token** |
| `\|G ∩ M\| == \|G\|` — containment | yes | no | **token** |
| what is this artifact's parent, and how deep is it? | no | no | **generation** |

Two consequences run through everything below.

**Cost should track what a viewer may see, and today it inverts.** At 10⁹ points a principal seeing
9.4% of the corpus costs the shipped path **1 635 ms** at whole-map zoom against 727 ms for one
seeing everything, because a narrow `M_auth` is a more fragmented row-space set and every pass is
`O(containers touched)`. The same two requests cost this design **0.49 ms and 13.5 ms**: the narrow
principal becomes the cheap one, because fewer artifacts survive the per-token pass.

**The cut cannot help, and does not need to.** It runs after the verdicts by construction — the
verdict is a per-artifact question with no lineage input
([decision 0080](../decisions/0080-the-frontier-is-a-per-artifact-test.md)) — so `artifact_budget`
bounds what is *served* and never what is *evaluated*. Nothing here changes that.

## 3. What is stored

**At the generation move**, beside the row form that already exists:

- **A hierarchical row-range index.** Rows are Morton rank, so the client's tile hierarchy *is* a
  hierarchy of row ranges. Each node holds the artifacts whose whole membership lies inside it, plus
  a subtree roll-up. **4.2 MB** at 10⁷ artifacts over 10⁸ points, 25.8 MB at 10⁶ over 10⁹, 3.2 s to
  build — beside a projection of the same population that costs 138 s.
- **A per-artifact extent**, `(min_row, max_row)` — 8 bytes, **80 MB** at 10⁷.
- **The lineage, with depth** — see §6. Mask-independent, and today rebuilt per request.
- **For a layer that partitions**, a **row-addressed label column** *instead of* the per-artifact
  bitmaps (§5).

**Per token**, the servable-label set [`architecture.md`](architecture.md) §8.5 already specifies —
built when the mask is composed, at session establishment. **This is the part of the design most
worth arguing about**, so §4.2 prices it on its own rather than asserting it.

## 4. The request

1. **Walk the index top-down.** Take a whole subtree where the viewport covers a node; descend only
   where it cuts one. Cost is the viewport's *perimeter*, not the population. Out come `settled` and
   `open`.
2. **`settled ∩ passes` is done.** These artifacts have `membership ⊆ viewport`, so *"has a visible
   member here"* and *"has a visible member"* are the same question and the token structure answered
   it. No intersection, no allocation, no mask probe. On clustered data the fixture measures **1.0
   row blocks per artifact**, so this is almost the whole population.
3. **`open ∩ passes`** takes the extent test (`viewport ⊇ [min,max]`, one `contains_range`), and
   whatever that misses pays the real masked `intersects`. This is the viewport's edge, and only its
   edge.
4. **Counts are a lookup.** Containment never runs.
5. **The artifact's own suppression still runs live**, unconditionally — verdict step 1, and nothing
   caches above it.

### 4.1 Why this is a candidate generator and not a decision

`MaskedSet::intersects_set` records that an early draft served an artifact *"wherever the box
intersected the viewport"*, which discloses the unmasked extent by panning. What is refused there is
using unmasked geometry as **the answer**. This uses it two ways and neither is that:

- **as a superset filter** — an artifact outside every node the viewport touches has no member
  there, so it cannot have a visible one. Skipping it withholds nothing.
- **as a containment fact** — `membership ⊆ viewport` makes the viewport question collapse into one
  answered from inside `M_auth` alone. **Exact, not conservative.**

The geometry decides only *which question to ask*. The probe asserts the served set is identical to
the shipped loop's, ordinal for ordinal, at every mask and every zoom — and, for the row-major route,
count for count as well.

### 4.2 What the per-token structure actually costs, and how much of it is needed

**Do not rest this on sharing.** §8.5's key is content-addressed on the auth data, so viewers holding
identical grants would share one copy — but identical grant sets are rare, so the honest assumption
is one structure per session and the figures below make it.

The comparison is not *840 ms at login against nothing*. It is **840 ms once against 840 ms every
time that viewer zooms out**, because that is what the two hoisted passes cost per request when they
are left in place. At 10⁷ artifacts, full mask:

| viewport | candidates | `masked_count` | containment | per request |
|---|---:|---:|---:|---:|
| whole map | 10 000 000 | 596 ms | 246 ms | **843 ms** |
| 6.25% | 705 662 | 40.4 ms | 16.5 ms | 56.9 ms |
| 0.39% | 44 166 | 2.8 ms | 0.8 ms | **3.6 ms** |
| 0.024% | 2 837 | 0.2 ms | 0.1 ms | 0.3 ms |

So it earns nothing at a narrow viewport — §4's index has already cut the candidates to hundreds —
and everything at a wide one. It breaks even on the first zoom-out, which on a map is usually the
opening view.

**Most of it is avoidable, and the two halves should not be hoisted together.** `masked_count` is 596
of the 843 ms and the *entire* 40 MB, and `ArtifactView::verdict` computes it unconditionally but
**tests** it only where the layer declares `require_member_visibility`. Without a criterion it is
simply the number served — needed for artifacts that survive the cut, which the budget bounds.

| layer | what the token must hold | build | residency |
|---|---|---:|---:|
| declares an existence criterion | the pass bitmap **and** a `u32` count per artifact | ~840 ms | 40 MB |
| declares none | the pass bitmap alone, count computed after the cut | **~246 ms** | **≤1.25 MB** |

Ten concurrent viewers of a criterion-free layer is then ~12 MB and ~2.5 s of one core spread across
their logins, rather than 400 MB and 8.4 s. ⊘ The split is not implemented; the figures are the
measured halves of a pass that today does both.

**And a viewer who never zooms out never needs it.** The structure can fill lazily — a narrow request
wants verdicts for a few hundred artifacts, so compute and keep those — which bounds the cost to what
a viewer actually looks at. That does not help the case where the opening view *is* the whole map,
which is the case that matters.

**The question underneath it is why a whole-map request evaluates 10⁷ artifacts at all.** The serving
loop runs every level of a treed layer, and at whole-map zoom a client can only draw the coarse one.
A request that evaluated only the levels it can serve from would face ~10³ artifacts there and would
not need the hoist. That changes what is *evaluated* and not what is served, but it reaches the
request contract, so it is a ruling rather than an optimisation — and the saving depends on the
hierarchy's shape. ⊘ Not measured.

### 4.3 Two things the construction needs to work at all

- **A hierarchy, not a granularity.** A flat index at 65 536-row blocks is worth nothing at mid-zoom:
  the viewport is then made of tiles smaller than the block, so no block is ever fully covered and
  nothing is settled. The tile-to-block relationship moves with corpus size, so any fixed block is
  wrong at most scales.
- **The extent beside the tree.** Node boundaries are powers of two, so an artifact at an arbitrary
  offset straddles one and is promoted a level, whose node the viewport must cover sixteen times as
  much of. Without the extent the `regions` arm gets nothing at a 6.25% viewport — measured.
- **An allocation-free walk.** Building a `Bitmap` per node merely to ask whether the viewport meets
  it costs nothing at 10⁸ rows and **5×** at 10⁹, where the hierarchy is two levels deeper.
  `range_cardinality` answers both the disjoint and the covered question from one call.

## 5. Three layouts, chosen by locality

`annotation-representation.md` §2.0 names three membership *sources*. The axis that decides cost is
a different one and does not line up with them: **row-space locality**.

| shape | what has it | blocks/artifact | §3's index |
|---|---|---:|---|
| clustered | HDBSCAN, point-and-radius, a hierarchy level | **1.0** | yes, entirely |
| regional | administrative boundary, spatial predicate | **1.0–1.6** | yes, via the extent |
| scattered | attribute predicate, per-analyst selection, term-as-artifact | **96.8** | **no** |

**"Large" and "numerous" are mutually exclusive**, which is why the middle row is not a third
problem: a boundary set measures 1.6 blocks per artifact at 10⁴ and 1.0 at 10⁶, because regions must
shrink as they multiply to keep fitting the same map.

A scattered artifact touches every node, so it is never inside one and never outside one. Under §3's
structures such a layer walls at ~**2×10⁵** artifacts — against 10⁷ clustered ones in the same
budget.

### 5.1 The inversion that removes the wall

The scattered shapes have a property clusters do not: **a single-valued attribute predicate
partitions the corpus.** Every point carries exactly one value, so the memberships are disjoint and
the natural storage is one label per **row**, not one bitmap per artifact. Candidacy becomes one scan
of `viewport ∩ M_auth` marking labels; the count becomes one histogram over `M_auth`. Both cost
**points rather than artifacts**.

Measured over 10⁸ points. The point is the columns, not the rows:

| viewport | artifacts | shipped | artifact-major | **row-major** |
|---|---:|---:|---:|---:|
| whole map | 10³ | 376 ms | **1.9 ms** | 333 ms |
| whole map | 10⁴ | 2 320 ms | **15.5 ms** | 332 ms |
| 6.25% | 10³ | 102 ms | 26.1 ms | **23.4 ms** |
| 6.25% | 10⁴ | 448 ms | 193 ms | **23.3 ms** |
| 0.39% | 10³ | 79.0 ms | 2.06 ms | **1.48 ms** |
| 0.39% | 10⁴ | 271 ms | 21.2 ms | **1.56 ms** |

**Ten times the artifacts and the row-major route does not move.** Its cost is ~4–5 ns per visible
row and nothing else — ⊘ ~280 ms *(modelled from that constant)* for a 6.25% viewport over 10⁹
points, however many artifacts the layer holds. The two layouts cross where you would want: row-major
is dearest at whole-map zoom, exactly where the extent test needs no scan at all. So the rule is
**take the cheaper**, and both are exact.

**The stronger argument is not speed.** At the target the artifact-major form does not fit. From the
residency campaign's measured 78.5 B per container on scattered membership, over 10⁹ rows:

| scattered artifacts | members each | artifact-major | row-major |
|---:|---:|---:|---:|
| 10⁴ | 10⁵ | 12.0 GB | **4.0 GB** |
| 10⁵ | 10⁴ | **78.5 GB** | **4.0 GB** |
| 10⁶ | 10³ | **78.5 GB** | **4.0 GB** |

⊘ Derived from the measured constant, not measured at 10⁹ — and consistent with what that campaign
saw directly, where its scattered arm at 10⁷ artifacts was OOM-killed rather than slow. The row-major
form is one `u32` per row whatever the artifact count, narrower at the `u8`/`u16` widths
`configuration.md` §1 already declares, and a mappable array rather than anonymous allocation.

**Not a new mechanism.** `artifacts-from-points` already *reads* this layout — an integer key column,
or a list column naming the artifacts a point belongs to — and converts it into artifact-major
bitmaps on the way in. The proposal is to keep what the build was handed, **row-addressed** rather
than entity-addressed: `attrs/` holds it by entity, which is right for a filter and wrong for a
viewport, and reaching the entity form costs an `entity_of` per row — ~120 ms of Stage 6's 175 ms was
that inversion rather than the counting.

⊘ **Single-valued only.** An overlapping layer needs a list per row, which is the same inversion at a
larger constant and is not measured.

## 6. The cut — built, not proposed

A principal who can see the whole corpus passes **every** artifact, so the cut is handed all 10⁷.
`artifact_cut_cost` had never run above 10⁶, where its own note said that scale "is not an operating
point this system serves"; that is false for exactly this principal.

| level | arm | lineage | cut, budgeted |
|---:|---|---:|---:|
| 10⁷ | treed, **before** | 44.9 ms | **1 008 ms** |
| 10⁷ | treed, **after** | 87.0 ms | **188 ms** |
| 10⁷ | flat | 6.1 ms | 28 ms |

**1 008 ms to 188 ms, peak RSS 1 078 MB to 470 MB, serving exactly what it served before** — checked
against the reference implementation the module is already tested against over random trees.

The plan materialised a lineage per frontier node: ~4.4 million of them, ~15 deep and almost entirely
shared, so 67 million entries and half a gigabyte to answer for a few thousand. Instrumentation put
**852 of the original 975 ms** there, against 12 ms for the frontier walk and 2 ms for the side
tables. Flattening them into one buffer was worth 975 → 440 ms on its own.

What replaced them follows from one observation: as the cut deepens, each lineage's pick walks *down*
it, so **a node is the pick over one contiguous range of depths and never again**, and the union
across the lineages sharing a node is an interval because they share its lower end. So the plan is one
`[from, until)` per servable node. Every depth's served count then falls out of a difference array —
the budget search compares counts instead of building a cut per candidate depth — and the served set
emerges ascending, so nothing sorts.

Three smaller changes went with it: `on_chain` is `climbed ∪ passing`, so the second climb over the
spine disappeared; the passing set is borrowed where it already arrives ascending, which the serving
path always produces; and **depth moved to `Lineage`**, which is why that column rose as the cut fell
— the work did not grow, it moved to the object that should be held per generation.

## 7. The numbers

**Clustered, 10⁷ artifacts, single-threaded, best of three:**

| viewport | shipped | design | |
|---|---:|---:|---:|
| whole map | 2 875 ms | **131 ms** | 22× |
| 6.25% | 819 ms | **9.8 ms** | 84× |
| 0.39% | 679 ms | **1.05 ms** | 650× |
| 0.024% | 672 ms | **0.29 ms** | 2 285× |

**Flat in the corpus size**, which is the property the whole construction is for — 10⁶ artifacts,
ten times the points:

| viewport | over 10⁸ points | over **10⁹ points** | shipped at 10⁹ |
|---|---:|---:|---:|
| whole map | 12.9 ms | **13.5 ms** | 727 ms |
| 6.25% | 1.92 ms | **2.05 ms** | 148 ms |
| 0.39% | 0.45 ms | **0.42 ms** | 101 ms |
| 0.024% | 0.24 ms | **0.18 ms** | 94.8 ms |

**The worst request end to end** — whole corpus, whole map, treed 10⁷-artifact layer:

| | before | now |
|---|---:|---:|
| verdict pass | ~135 ms | ~135 ms |
| lineage build *(belongs per generation)* | 45 ms | 87 ms |
| cut | ~1 010 ms | **188 ms** |
| **total** | **~1 190 ms** | **~410 ms, or ~325 ms once the lineage is cached** |

⊘ **10⁷ artifacts over 10⁹ points is not measured directly** — the entity-space fixture for it needs
~41 GB against 47 GB of RAM. Two independent measurements agree on ~135 ms for the verdict there: the
design is flat in corpus size (10⁶ artifacts, 10⁸ → 10⁹ points, no movement) and linear in artifact
count (13.5 ms at 10⁶ over 10⁹).

## 8. What this depends on, and is not yet true

Two live defects. Neither is about scale, and the design is worth little without them.

**8.1 Any artifact write invalidates every cached row form in every view.** `ArtifactStore`'s
`version` is global and `ProjectionKey` carries it, so one suppression, one grown membership or one
publication anywhere rebuilds every layer's projection everywhere — **138 s at 10⁷ artifacts**
(16 s at 10⁶). Under read-write load the cache never survives to be used, and the per-token structure
would inherit the same fate. It needs to be per `(layer, level)`, and ideally patched rather than
rebuilt: a write touches a handful of ordinals.

**8.2 `Lineage` is rebuilt on every request**, per level, under the artifacts lock, from something
that depends on neither the mask nor the viewport. It is now 87 ms at 10⁷ because it carries depth —
which is the right home for depth, and the wrong cadence for the object.

**8.3 §8.5's cache key is incomplete, and the gap is fail-open.** As specified it is *(auth-data
hash, auth-plugin version, overlay version)*, and none of those moves when an **artifact** does. A
generating set that grew has more to contain, so a cache keyed only on the viewer keeps serving a
label the viewer no longer contains — and growth makes containment *harder*, so the stale answer is
the permissive one. The key needs the artifact store's version beside the overlay's;
`ProjectionKey` already carries exactly that one layer down. Worth writing into §8.5 whether or not
any option here is taken.

**8.4 What the request still allocates after the verdict.** The serving loop collects
`passing: Vec<(u32, EntityId, u64, Option<u32>)>` — every artifact that cleared the predicate — and
hands its ordinals to the cut. At 10⁷ passing that is **~240 MB allocated and freed per request**,
and it is in no figure here. The routes in §4 leave the passing set as a bitmap, so materialising is
a *choice*; what stands in the way is that `cut` takes a slice. ⊘ Unmeasured, and it is the next
reduction: the cut's remaining 188 ms is nine memory-bound passes over the level with no dominant
term, so what is left is structural rather than local.

## 9. The ruling this asks for

**Whether a layer with no column and no row-space locality carries a declared bound** — refused,
warned, or merely reported. §5.1 removes the wall for anything that partitions, so what is left
un-helped is a layer that is scattered **and** overlapping **and** numerous: an enumerated set with
no column. Every real instance of that shape is human-made or vocabulary-made, and the realistic
counts are thousands, where the cost is 1.5–15 ms.

Three routes, and the house rule reads clearly here — nothing leaks, nothing is irreversible, and a
wrong guess costs a rebuild:

- **(a) Row-major wherever the layer partitions.** §5.1. Removes the wall rather than bounding it.
- **(b) A declared bound on the rest.** A warning that prints the numbers, not a refusal.
- **(c) Report the shape in the build's frame report.** Blocks per artifact is one number and the
  build already computes the row form it comes from, so a layer that will be slow says so when it is
  built rather than when it is panned.

**Recommended: (c) always, (a) wherever the layer partitions, (b) as a warning.**

## 10. What is not in scope, and what is not measured

- **Nothing changes the client contract.** No option above requires a new request field; (b) would
  add a config-time declaration, not a wire change.
- **Nothing changes the write path.** The index is derived from the row form and built where it is
  built. §8.1's fix makes writes cheaper, never harder.
- **The routes are probe-side.** Every quantity goes through the shipped structures —
  `ArtifactRows::build`, `intersects`, `masked_count`, `satisfied_rank`, the three-set composed
  arithmetic `EffectiveMask` does, viewports from `tiles_for_bbox`. The control flow is written in
  the probe, not in `viewport.rs`. The cut in §6 is the exception: that is in the engine.
- **One level, one layer.** A treed layer's loop runs per level and a request may name several
  layers; a hierarchy does not multiply the cost, because each level's cost is proportional to its
  own population and the figures are for a total — but several *layers* do.
- **Not a real clustering.** The arms bracket it; real membership is 14–170× cheaper per member than
  the synthetic arm.
- **Selection functions** — rank by masked size, filter by label text — become cheap once the
  per-token structure exists, because the masked count is then a lookup. The ranking contract itself
  is unruled and is not proposed here.
