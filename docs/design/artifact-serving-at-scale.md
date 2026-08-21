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

- **A containment partition** — one byte per artifact naming the boolean expression over *terms*
  that decides it, plus a bitmap per distinct expression. **10 MB at 10⁷**, build-time, and it names
  no principal, so it is shared by all of them. This is what replaces §8.5's per-token servable-label
  set; see §4.2.

**Per token: nothing**, except for a layer declaring an existence criterion — see §4.3.

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

### 4.2 Containment does not need a per-token structure at all

**There are a great many tokens** (owner, 2026-08-21), so anything materialised per token over the
artifact population is the wrong shape however cheap one copy is. `architecture.md` §8.5 specifies a
servable-label set on that cadence; **for containment it is not needed**, and the design says why in
`annotations.md` §4 already:

> what decides is **which** terms, never *how many* items — which is why terms are what make the
> test tractable (**I5**)

Containment is `G ⊆ M_auth`, and an entity is in `M_auth` exactly when its own visibility expression
holds for the principal's terms. So

```text
G ⊆ M_auth   ⟺   ( ⋀ vis(e) for e in G )( T )
```

— a boolean expression over terms with **nothing about the mask in it**. Compose it once at build,
canonicalise, intern: artifacts sharing an expression share an answer for every principal that will
ever exist. §7.8's per-term generating set is what keeps the number of distinct expressions small —
a sample drawn from inside one signature group composes to *holds that group's term* — so the count
is the vocabulary's rather than the layer's.

Per request that is either a union of the satisfied groups' bitmaps, or a byte lookup per candidate,
whichever the viewport has left smaller. **Measured against the per-token route at 10⁷ artifacts,
full mask:**

| viewport | shipped | per-token *(151 ms setup)* | **build-time groups *(no setup)*** |
|---|---:|---:|---:|
| whole map | 2 929 ms | 141.9 ms | **140.0 ms** |
| 6.25% | 891 ms | 10.1 ms | **9.87 ms** |
| 0.39% | 692 ms | 0.845 ms | **0.869 ms** |
| 0.024% | 666 ms | 0.186 ms | **0.221 ms** |

**Parity, with the per-token structure deleted.** And at a narrower principal it is ahead outright —
a 9.4% mask at whole-map zoom is **4.09 ms against 13.1 ms** — because the groups a principal fails
are never touched, where the per-token pass had to evaluate every artifact once to find that out.

Storage is one byte per artifact plus the group bitmaps: **40 MB at 10⁷**, build-time, and it names
no principal, so one copy serves all of them however many tokens there are.

**Two forms of the same fact, and the route picks between them on size**, exactly as §5's two layouts
do: unioning the satisfied groups is `O(containers)` and independent of the viewport, ~6 ms at 10⁷;
testing each candidate through the byte table is `O(candidates)`, which at a 0.024% viewport is three
hundred of them. Taking the union unconditionally cost 626 µs at a viewport whose answer was 73 µs.

⊘ **Two things the arm does not model.** A **suppression** removes a member of `G` from `M_auth`
whatever the terms say, so the answer must be intersected with *no member suppressed* — an inverted
index from entity to the artifacts whose generating set holds it, refreshed when the **overlay**
changes rather than per token. And a generating set that lost members in projection can never be
contained, which is per view and mask-independent, so it folds into the group.

### 4.3 The masked count, which is the part that does not fully dissolve

`|membership ∩ M_auth|` is genuinely per-principal. It splits by what the layer declares:

- **No existence criterion** — the count is only the number *beside* a served artifact, so it is
  needed for what survives the cut. `O(budget)`: measured at **22–205 µs** for a thousand artifacts,
  against 596 ms for ten million. Solved.
- **An existence criterion** — the count decides whether the artifact exists for this viewer, so it
  is needed per candidate. The viewport bounds that: **0.2 ms at a 0.024% viewport and 2.8 ms at
  0.39%** over 10⁷ artifacts, but **596 ms at whole-map zoom**.

So one case remains: **a criterion layer at a wide viewport**. Three ways out, and the third is the
one worth ruling on:

- Hold the counts per token after all — which is what §8.5 buys, and what many tokens make
  expensive. It is now the *only* thing that structure would be for.
- Keep, per artifact, a count **per signature group** — build-time and mask-independent, summed over
  the satisfied groups per request. ⊘ Not measured, and the storage is real: an artifact spanning
  thirty-two groups is ~200 bytes, so ~2 GB at 10⁷.
- **Evaluate only the levels the request can serve from.** A whole-map request runs every level of a
  treed layer when the client can draw the coarse one; bounded to what it can serve, the wide case is
  ~10³ artifacts and the question does not arise. This changes what is *evaluated* and not what is
  served, but it reaches the request contract. ⊘ Not measured.

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
| whole map | 2 988 ms | **31.7 ms** | 94× |
| 6.25% | 904 ms | **8.2 ms** | 111× |
| 0.39% | 725 ms | **0.96 ms** | 755× |
| 0.024% | 713 ms | **0.28 ms** | 2 547× |

Those are the **build-time-groups** route, so nothing above is amortised over a session: every row is
what one request costs on a cold token.

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
| verdict pass | ~140 ms | **~32 ms** |
| lineage build *(belongs per generation)* | 45 ms | 87 ms |
| cut | ~1 010 ms | **3.05 ms** |
| **total** | **~1 195 ms** | **~131 ms, or ~35 ms once the lineage is cached** |

⊘ **10⁷ artifacts over 10⁹ points is not measured directly** — the entity-space fixture for it needs
~41 GB against 47 GB of RAM. Two independent measurements agree on ~135 ms for the verdict there: the
design is flat in corpus size (10⁶ artifacts, 10⁸ → 10⁹ points, no movement) and linear in artifact
count (13.5 ms at 10⁶ over 10⁹).

### 7.1 What a request pays, stage by stage

**The worst case is what bounds the system**, since it fixes how many viewers one core carries.
10⁷ artifacts, treed layer, a principal who sees the whole corpus, single-threaded, no per-token
state:

| stage | whole map | 6.25% | 0.39% | 0.024% |
|---|---:|---:|---:|---:|
| verdict — index walk, containment, viewport edge | 31.7 ms | 8.2 ms | 0.96 ms | 0.28 ms |
| masked count, no criterion — `O(budget)` | 0.06 ms | 0.04 ms | 0.07 ms | 0.04 ms |
| lineage *(per generation; §8.2)* | ~96 ms | ~96 ms | ~96 ms | ~96 ms |
| **cut** | **3.05 ms** | 60 ms | 43 ms | 23 ms |
| total | ~131 ms | ~164 ms | ~140 ms | ~119 ms |
| **with the lineage held per generation** | **~35 ms** | ~68 ms | ~44 ms | ~23 ms |

**The worst request is now the cheapest**, which is the shape the whole design has been converging
on: a principal who can see everything passes everything, and passing everything is exactly what
makes both the containment groups and the downward walk collapse. The expensive request is now a
*mid-zoom* one for a principal who sees most but not all — where the viewport does not narrow much
and the walk declines.

Against **~1 190 ms** for the same request before this campaign. Two of the four lines got there in
this round and both were the same mistake — materialising something to look at a fraction of it:

- **The verdict was 140 ms and is 31.7**, because it collected ten million ordinals out of a bitmap
  and then **sorted** them. Both inputs are already ascending, so the sort was 110 of the 140 ms and
  bought nothing. What remains is the materialisation itself, which exists only because `cut` takes
  a slice.
- **The cut was 254 ms and is 199**, because it built one interval per *servable* node — three
  arrays of ten million, 120 MB — and scanned all of them to select the 729 a budget serves. It now
  reads only the depth buckets a cut can reach.

**The cut is no longer the bound.** It was `O(level)` — five sequential passes over the ordinal
space — and it is now `O(budget × branching)` for the principal that bounds the system, by walking
down from the roots instead of sweeping across:

| 10⁷ level, passing | lineage | cut, cold | **cut, warm** |
|---:|---:|---:|---:|
| 2 838 | 148 ms | 23.6 ms | 23.1 ms |
| 44 248 | 101 ms | 44.6 ms | 42.8 ms |
| 714 286 | 95 ms | 62.7 ms | 59.8 ms |
| **10 000 000** | 96 ms | 117 ms | **3.05 ms** |

**A budget settles on a shallow depth** — a thousand artifacts is depth six in a three-way tree — so
the answer lives in the top of the tree while every earlier revision swept the level to find it.
With every node above the cut passing, the rule collapses: a node above it has a passing child so
something deeper represents it; a node *at* it has none so nothing does; and every node has a passing
parent so none is its lineage's fallback. The served set is exactly the nodes at that depth, plus the
leaves shallower than it, which have nothing deeper to replace them.

**Where a node above the cut fails, the walk declines** and the sweep answers at the cost it always
had. That is the residual: a mask that is broad **and** fragmented — many artifacts passing, with
failures scattered near the top. It is not the shape that bounds throughput, because a principal who
passes everything leaves no fragmentation at all, and a principal who fails much is one for whom the
sweep is already proportional to the little they can see. Both routes are checked against the same
reference implementation over random trees rather than against each other, and a test asserts the
walk is actually taken rather than silently declining everywhere.

**Cold against warm is the lineage's question, not the cut's.** The walk reads a child index that is
a property of the tree, built on first use and reused after. A lineage held per generation sees only
the warm column; today it is rebuilt per request, so the cold column is what a request pays and
§8.2 is the difference between them.

⊘ **Two things measured and reverted**, recorded so they are not re-attempted: pre-sizing the depth
buckets from a counting pass (193 ms against 198 at full passing, and **32 against 24** at a level
where a few thousand pass — the second sequential scan costs more than the growth it avoids), and
`ArtifactRows::intersects`' early exit at whole-map zoom, which loses about a tenth where every
artifact meets the viewport anyway.

⊘ **Not in this table**: the gather, the record-blob reads for supplied content, and the wire
encoding.

### 7.2 The whole picture, by artifact type and principal

**10⁹ points, 10⁷ artifacts, single-threaded.** Per-request totals in milliseconds, with the lineage
held per generation (§8.2 — without it add ~96 ms to every cell).

**Per session: nothing.** The containment partition (§4.2) is build-time and names no principal, so a
token costs what it always did — the mask fragment — and no artifact structure at all. That is the
change this campaign made that matters most at a large token count.

**Per generation, per view:** the row form (3.6 GB, unchanged), the tile index and extents (~84 MB),
the containment partition (~40 MB), the lineage with depth and child index (~200 MB).

#### Clustered and regional — HDBSCAN, point-and-radius, boundaries, hierarchy levels

| principal sees | whole map | 6.25% | 0.39% | 0.024% |
|---|---:|---:|---:|---:|
| everything | **35** | 68 | 44 | 23 |
| 9.4% | 64 | 46 | 24 | 23 |
| 3.1% | 56 | 43 | 24 | 23 |

The verdict half of those is measured; the cut half is interpolated from the measured ladder against
the candidate count each cell produces. **Everything is between 23 and 68 ms**, and it is the cut's
sweep floor rather than the verdict that decides almost every cell — the verdict itself ranges from
0.03 ms to 31.7.

#### Scattered and partitioning — a single-valued attribute predicate

Served row-major (§5.1), at ~4–5 ns per visible row of the viewport and **flat in the artifact
count**. ⊘ Derived from that measured constant at 10⁹ points; the constant is measured at 10⁸.

| principal sees | whole map | 6.25% | 0.39% | 0.024% |
|---|---:|---:|---:|---:|
| everything | ~50 † | ~280 | ~18 | ~1 |
| 9.4% | ~50 † | ~26 | ~2 | ~0.2 |
| 3.1% | ~50 † | ~9 | ~1 | ~0.1 |

† at whole-map zoom the extent test settles the layer without a scan, so this is the artifact-major
route and the artifact count decides it, not the corpus. **Mid-zoom is this shape's worst case**,
inverting the clustered one, because that is where the viewport is large in rows and small in
settled blocks.

#### Scattered and overlapping — per-analyst selections, terms-as-artifacts

**Not servable at this scale, and no layout here changes that.** Every artifact touches every node,
so none is ever settled and each pays a masked intersection: ~2.4 µs apiece, measured, which is
**~24 s** at 10⁷. The wall is around **4×10⁵ artifacts** for a sub-second budget at 10⁹ points.

That is the ruling §9 asks for. It is not a gap in the design so much as a shape nothing can serve:
a million arbitrary overlapping sets over a billion points, each spread across the whole map, is not
a picture. Every real instance — an analyst's selection, a vocabulary term — is made by a person or a
controlled list, and lands in the thousands, where the cost is 2.4–24 ms.

### 7.3 What bounds the system now

**The cut's sweep floor, at ~23 ms.** The downward walk (§6) applies only where more than half the
level passes, which after viewport filtering is the whole-map request alone; everywhere else the
sweep runs and it is `O(level)` — 23 ms at a 10⁷ level even when two thousand artifacts pass.

So the ordering has inverted twice over. The whole-corpus principal at whole-map zoom, which began
this campaign as the worst request at ~1 190 ms, is now the **cheapest** at ~35 ms, because passing
everything is exactly what makes the containment groups and the downward walk collapse. What is
expensive now is a **mid-zoom request for a broad principal** — 68 ms — where the viewport does not
narrow much, the walk declines, and the sweep pays for a level the request barely touches.

⊘ **The next reduction is the sweep's floor**, and it is bookkeeping rather than design: the sweep
scans `0..span` to find the on-chain nodes, which could be collected as a list during the frontier
climb it already performs. That would make it `O(passing)` and take the floor to single-digit
milliseconds, putting every cell above under ~35 ms. Not attempted.

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
