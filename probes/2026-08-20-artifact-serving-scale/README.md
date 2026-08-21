# Serving artifacts at scale: where the request path spends its time, and what removes it

**Date:** 2026-08-20 · **Harness:** `cargo run --release --bin artifact_serving_scale`
· **Host:** WSL2, 47 GB RAM, 12 cores · **Driver:** [`run.sh`](run.sh) · **Data:** [`data/`](data/)

The target is **10⁷ artifacts over 10⁹ points, served inside a second, on one core** — one core
because a serving node must carry ten or more simultaneous viewers, so a per-request budget met by
spending the machine is not met at all.

`artifact-delivery.md` Stage 8 owes exactly this figure and nothing has ever measured it. The
neighbouring quantities are measured — the fold's artifact pass (32.8 s on eight threads), residency
(3.6 GB on the realistic arm), the cut (0.6 ms per ten thousand visible) — and the pass between them
is not.

## The result

**The shipped request path is `O(artifacts)` four times over, and none of the four has the request in
it.** Replacing the population scan with a hierarchical row-range index over artifacts, and hoisting
the mask-dependent half into the per-token structure `architecture.md` §8.5 already specifies, is
**21× at the worst request shape and 100–3000× at ordinary ones** — single-threaded, with every
verdict identical to the shipped loop's, asserted rather than argued.

**At 10⁷ artifacts over 10⁸ points**, single-threaded, best of three — the target's own population:

| principal sees | viewport | shipped | **indexed + per-token** | |
|---|---|---:|---:|---:|
| everything | whole map | 2 875 ms | **131 ms** | 22× |
| everything | 6.25% | 819 ms | **9.8 ms** | 84× |
| everything | 0.39% | 679 ms | **1.05 ms** | 650× |
| everything | 0.024% | 672 ms | **0.29 ms** | 2 285× |
| 9.4% | whole map | 2 712 ms | **62.7 ms** | 43× |
| 3.1% | whole map | 2 364 ms | **25.5 ms** | 93× |

**At 10⁶**, where each artifact carries a hundred members rather than ten:

| principal sees | viewport | shipped | **indexed + per-token** | |
|---|---|---:|---:|---:|
| everything | whole map | 479 ms | **12.9 ms** | 37× |
| everything | 0.39% | 91.2 ms | **0.45 ms** | 205× |
| 3.1% | whole map | 1 056 ms | **0.33 ms** | 3 200× |

Two things in that table are worth more than the ratios.

**Cost tracks containers touched on both sides, and the mask side can dominate.** At 10⁶ artifacts
of a hundred members each the shipped path gets *slower* as the principal narrows — 479 ms at a full
mask against 1 056 ms at 3.1% — because a narrow `M_auth` is a more fragmented row-space set and
every one of the four passes walks it. It is not universal: at 10⁷ artifacts of ten members each the
artifact side is one container whatever the mask does, and the sweep runs slightly *faster* as the
mask narrows. What is universal is that the shipped path never gets cheaper **in proportion to what
a viewer may see**. The routes here do — 22× at a full mask, 93× at 3.1% — because a narrower
principal leaves fewer artifacts alive after the per-token pass.

**The remaining worst case is a broad principal at whole-map zoom**, the one request where neither
the viewport nor the mask removes anything: **131 ms at 10⁷ artifacts**.

**And it is flat in the corpus size.** Ten times the points at a fixed artifact count moves nothing:

| viewport | 10⁶ artifacts / 10⁸ points | / **10⁹ points** | shipped at 10⁹ |
|---|---:|---:|---:|
| whole map | 12.9 ms | **13.5 ms** | 727 ms |
| 6.25% | 1.92 ms | **2.05 ms** | 148 ms |
| 0.39% | 0.45 ms | **0.42 ms** | 101 ms |
| 0.024% | 0.24 ms | **0.18 ms** | 94.8 ms |

That is the property the whole construction is for: cost is a function of the artifacts and the
viewport, not of the corpus beneath them. The shipped path is not flat in it — at 10⁹ points a
principal seeing 9.4% of the corpus costs it **1 635 ms** at whole-map zoom against 727 ms for one
seeing everything, and this design 0.49 ms against 13.5 ms.

**One correction the 10⁹ tier forced, recorded because it was invisible below it.** The first walk
built a `Bitmap` per node just to ask whether the viewport met it — a `malloc` on every node of
every descent. At 10⁸ rows that cost nothing measurable; at 10⁹, where the hierarchy is two levels
deeper and a narrow viewport descends all of it, it was **5×** (1.46 ms against 0.29 ms at a 0.024%
viewport). `range_cardinality` answers both the disjoint and the covered question from one call and
allocates nothing.

## What the four passes are

```text
for ordinal in 0..rows.len() {                                    // ← every artifact
    if !rows.intersects(ordinal, &tile_rows, mask) { continue }   // 1. candidacy
    ... masked_count(ordinal, mask)                               // 2. the number and the criterion
        satisfied_rank(ordinal, mask, ...)                        // 3. containment
}
lineage = Lineage::new(store.level(&name, level).map(...))        // 4. every parent pointer, per request
cut(&lineage, &passing, budget, prune)
```

Passes 2 and 3 are functions of `M_auth` and the layer — **there is no viewport in either**. Pass 4
is a function of the layer alone and has neither. Only pass 1 is genuinely a per-request question,
and even it is asked of every artifact rather than of the ones the viewport could reach.

## The two structures

### A hierarchical row-range index — the viewport half

Rows are Morton rank, so a tile is a contiguous row range and the client's tile hierarchy *is* a
hierarchy of row ranges. The index holds, per node, the artifacts whose whole membership lies inside
it. A request walks it top-down, taking whole subtrees where the viewport covers a node and
descending only where it cuts one, so the cost is the viewport's **perimeter** rather than the
population.

It is used two ways and neither is the shape `MaskedSet::intersects_set` refuses:

- **as a superset filter** — an artifact outside every block the viewport touches has no member
  there at all, so it cannot have a *visible* one. Skipping it withholds nothing.
- **as a containment fact** — an artifact wholly inside a fully covered node has
  `membership ⊆ viewport`, so *"has a visible member here"* and *"has a visible member"* are the
  same question, and the second has no viewport in it. **This half is exact, not conservative**, and
  it is where the large factors come from: on the clustered arm the fixture measures **1.0 row
  blocks per artifact**, so almost the whole population is settled without a single per-artifact
  operation.

What is refused in the design is serving an artifact *"wherever the box intersected the viewport"* —
using unmasked geometry as the **answer**, which discloses the unmasked extent by panning. Here the
geometry decides only *which question to ask*; every artifact served still cleared the masked test,
and the probe asserts the served set is identical ordinal for ordinal at every mask and every zoom.

**One granularity is not enough, and that is measured rather than assumed.** A flat index at
65 536-row blocks answers a whole-map request in 1.6 ms and a 6.25% request in 2.2 ms — no better
than no index — because at that zoom the viewport is made of tiles *smaller* than the block, so no
block is ever fully covered and nothing is ever settled. The tile-to-block relationship moves with
the corpus, so a fixed block is right at one scale and wrong at every other.

### The servable-label set — the mask half

`architecture.md` §8.5 already specifies it: *"the servable-label set (containment decisions), keyed
by (auth-data hash, auth-plugin version, overlay version), invalidated by overlay change"*, and
*"because containment binds to `M_auth` rather than `M_sel`, the servable-label set is computed once
per token and reused across every keystroke."* **It is specified and unbuilt.** This probe measures
what building it is worth.

Two quantities, neither with a viewport in it: the containment verdict `|G ∩ M| == |G|`, and the
masked count `|membership ∩ M|`. Computing both once when the mask is composed turns passes 2 and 3
from per-request into per-token, and the key is content-addressed on the *auth data* — so viewers
holding the same grants share one copy rather than each building their own.

**Filters compose with it for free**, and that is a property of the invariant rather than luck:
`MaskedSet` is deliberately blind to any attribute filter (**I12** — a filter may move the frontier
up, never down), so a viewer typing in a search box does not invalidate any of it.

## The fixture, and three corrections it needed

Every number here comes from the structures the engine ships — a real `RowSpace`, records through
`ArtifactRows::build`, `ArtifactRows::intersects`/`masked_count`/`satisfied_rank`, the same
three-set composed-mask arithmetic `EffectiveMask` does, and viewports from the shipped
`tiles_for_bbox`. Three earlier revisions measured a system nobody has, and each correction moved
the headline by more than any option did:

- **Membership generated in entity space and permuted into row space.** Backwards. Entity ids are
  allocated in signature-then-Morton order and rows are Morton rank, so a spatially coherent cluster
  is contiguous *in row space* and it is the entity form that is scattered. Generating in the wrong
  space scattered every artifact across ~64 row blocks, collapsed both arms onto the pessimistic
  one, and made the whole-map loop **12× slower than it really is**.
- **A random permutation.** No corpus has one. Entity space is *G* contiguous signature groups, each
  internally Morton-ordered; the fixture uses 32, which puts the entity/row ratio in the 11.8× the
  storage campaign measured.
- **Viewports as evenly-sized ranges strided across the space.** A viewport is a rectangle, and
  Morton decomposition gives a few long runs plus a fringe of short ones. The synthetic version made
  candidacy *rise* as the viewport narrowed, and made whole-block coverage impossible at every zoom
  but the widest — which is precisely the quantity the index turns into free answers.
- **Generating sets drawn across all signature groups.** Containment is not a coverage fraction, so
  a sample spanning 32 groups is served only to a principal holding all 32 — every arm below a full
  mask measured a layer that served nothing to anybody. §7.8's per-term generating set is what the
  design says to build, and with one the pass rate tracks the mask.

## The shapes that have no locality, and where they wall

Half the model's artifact kinds are not clusters. `annotation-representation.md` §2.0 names three
membership *sources*; the axis that decides every cost here is a different one and does not line up
with them — **row-space locality**, measured as blocks per artifact:

| arm | what has this shape | blocks/artifact |
|---|---|---:|
| `runs` | HDBSCAN, a point-and-radius blob, a level of a hierarchy | **1.0** |
| `regions` | administrative boundary, spatial predicate | **1.0–1.6** |
| `scattered` | attribute predicate, per-analyst selection, term-as-artifact | **96.8** |

A scattered artifact touches every node of the tree, so it is never inside one and never outside
one: the walk hands the whole population back and each artifact pays a masked intersection.
Measured over 10⁸ points, single-threaded:

| scattered artifacts | whole map | mid-zoom (6.25%) — the worst |
|---:|---:|---:|
| 10³ | 1.6 ms | 2.1 ms |
| 10⁴ | 16.0 ms | 24.5 ms |
| 10⁵ | 159 ms | **243 ms** |

Linear, so the wall is around **2×10⁵** — against 10⁷ for clustered artifacts inside the same
budget. Whole-map is *cheaper* than mid-zoom here, inverting the clustered case, because at whole
map the viewport contains everything and the extent test settles the entire population.

**"Large" and "numerous" are mutually exclusive**, which is why `regions` is not a third problem: a
boundary set measures 1.6 blocks per artifact at 10⁴ and 1.0 at 10⁶, because the regions have to get
smaller as they get more numerous to keep fitting the same map.

## And a different layout for the ones that are everywhere

The scattered shapes have a property clusters do not: **a single-valued attribute predicate
partitions the corpus.** Every point carries exactly one value, so the memberships are disjoint and
the natural storage is one label per **row** rather than one bitmap per artifact. That inverts both
questions — candidacy becomes one scan of `viewport ∩ M_auth` marking labels, and the count becomes
one histogram over `M_auth` — and both then cost points rather than artifacts.

Measured over 10⁸ points, single-threaded, both against the same per-token structure. **The point is
the columns, not the rows:**

| viewport | artifacts | shipped | artifact-major (the index) | **row-major (a label per row)** |
|---|---:|---:|---:|---:|
| whole map | 10³ | 376 ms | **1.9 ms** | 333 ms |
| whole map | 10⁴ | 2 320 ms | **15.5 ms** | 332 ms |
| 6.25% | 10³ | 102 ms | 26.1 ms | **23.4 ms** |
| 6.25% | 10⁴ | 448 ms | 193 ms | **23.3 ms** |
| 0.39% | 10³ | 79.0 ms | 2.06 ms | **1.48 ms** |
| 0.39% | 10⁴ | 271 ms | 21.2 ms | **1.56 ms** |
| 0.024% | 10⁴ | 259 ms | 5.07 ms | **0.18 ms** |

Ten times the artifacts and the row-major route does not move. Its cost is ~4–5 ns per visible row
in the viewport and nothing else. The two cross where you would want them to — row-major is dearest
at whole-map zoom, where it walks the corpus and where the extent test needs no scan at all — so the
rule is *take the cheaper*, and both are exact.

**Candidates and counts are asserted identical to the shipped loop's** — ordinal for ordinal and
count for count, every artifact, before either route is timed.

### And a list per row where the layer overlaps

The label column above needs each point to carry exactly one value. A per-analyst selection, a
terms-as-artifacts layer or a multi-valued predicate carries several or none, so the row-major form
is `row → list of artifacts`. Everything else is the same, and `artifacts-from-points` already reads
this shape — a list column naming the artifacts a point belongs to — before converting it into
bitmaps.

Measured over 10⁵ **scattered and overlapping** artifacts, 10⁷ points, against the best
artifact-major route:

| viewport | shipped | artifact-major, hoisted | **row-major list** |
|---|---:|---:|---:|
| whole map | 1 307 ms | **20.2 ms** | 75.7 ms |
| 6.25% | 478 ms | 29.4 ms | **6.3 ms** |
| 0.39% | 176 ms | 36.8 ms | **0.96 ms** |
| 0.024% | 32.5 ms | 16.1 ms | **0.14 ms** |

Candidates asserted identical to the shipped loop's before either is timed. The crossover is the
same as the label column's and for the same reason — a row-major scan is cheapest where the viewport
is small, and at whole-map zoom the extent test settles the layer without scanning anything.

**Its cost is memberships in the viewport**, so it is flat in the artifact count and linear in `k`,
the average artifacts a point belongs to: `O(k × visible rows)` at ~10 ns a row. That is the right
invariant for a layer covering a corpus, where `k` is fixed and the artifact count is not. The arm
above holds *members per artifact* fixed instead, so its totals grow with the artifact count — the
fixture's choice, not the layout's.

⊘ At 10⁹ points a 6.25% viewport is 6×10⁷ visible rows, so ~600 ms at that constant *(derived)*. The
awkward band for this shape is therefore mid-zoom: narrow viewports are tens of milliseconds and
whole-map is answered artifact-major.

### The part that is not about speed

At the target the artifact-major form does not fit. From the residency campaign's measured 78.5 B
per container on scattered membership, over 10⁹ rows:

| scattered artifacts | members each | artifact-major | row-major |
|---:|---:|---:|---:|
| 10⁴ | 10⁵ | 12.0 GB | **4.0 GB** |
| 10⁵ | 10⁴ | 78.5 GB | **4.0 GB** |
| 10⁶ | 10³ | 78.5 GB | **4.0 GB** |

⊘ Derived from the measured constant, not measured at 10⁹ — and consistent with what the residency
campaign saw directly, where the scattered arm at 10⁷ artifacts was OOM-killed rather than slow. The
row-major form is one `u32` per row whatever the artifact count, narrower at the `u8`/`u16` widths
`configuration.md` already declares, and a mappable array rather than anonymous allocation.

## Composing the mask with the viewport once instead of once per artifact

`ArtifactRows::intersects` asks *does this artifact have a visible member in view* by materialising
`membership ∩ viewport` and putting that through the composed mask — three set operations and a heap
allocation, **per artifact**. But `viewport ∩ M_auth` has no artifact in it. Composed once per
request, each artifact is left with a single `Bitmap::intersect`: a boolean with an early exit that
stops at the first container that meets.

**It matters most for the shape it was worst for.** A scattered artifact has members everywhere, so
it almost always *does* meet the viewport — 99.8% of them at a hundred members and a 6.25% viewport
— and the 2.4 µs test was confirming a foregone conclusion the long way round.

Measured, 10⁵ scattered artifacts over 10⁸ points:

| viewport | shipped | per-artifact composition | **hoisted** |
|---|---:|---:|---:|
| whole map | 1 307 ms | 20.9 ms | 20.8 ms |
| 6.25% | 478 ms | 164 ms | **29.8 ms** |
| 0.39% | 176 ms | 60.0 ms | **38.3 ms** |
| 0.024% | 32.5 ms | 24.7 ms | **16.7 ms** |

Exact, and trivially so: `rows ∩ (viewport ∩ M_auth) ≠ ∅` and `rows ∩ viewport ∩ M_auth ≠ ∅` are the
same statement. What changes is where the composition happens.

**What is left is ~250 ns an artifact of cache misses** — the containment byte, the row form's
pointer, and the bitmap's first container are three random accesses into three structures — so a
purely per-artifact route over a scattered layer walls around 4×10⁶ artifacts rather than 4×10⁵.
Beyond that the layout has to change, which is what the row-major section above is about.

## A real hierarchy, where the membership comes from the tree

Every arm above lays a tree over the ordinal space while giving each artifact a membership near its
own ordinal, which makes the two unrelated — a viewport can drop the whole top of the tree and leave
its leaves in view. **No hierarchy can do that**: a parent cluster contains its children, so a parent
is in view whenever any child is and the root is in view always. The `nested` arm builds membership
from the tree instead: the root owns the row space and each node splits its range among its
children.

**It is cheap to store, which is worth stating first.** Every level covers the corpus, so the layer's
membership is `depth × rows` — but each node is a *contiguous range*, so it is one run whatever its
size, and the whole thing is `depth × (rows / 65 536)` containers. Measured at **1.2 row blocks per
artifact** over 10⁶ nodes on a 10⁹-row corpus.

At 10⁶ artifacts over 10⁹ points, milliseconds, by principal coverage and viewport:

| | 100% | 75% | 50% | 25% | 6.25% | 0.39% | 0.024% |
|---|---:|---:|---:|---:|---:|---:|---:|
| **100%** | 14.3 | 87.8 | 71.5 | 33.8 | 13.6 | 4.0 | 1.9 |
| **75%** | 72.7 | **102.2** | 89.4 | 58.4 | 37.5 | 25.9 | 14.3 |
| **50%** | 61.5 | 87.8 | 76.9 | 53.0 | 33.3 | 25.2 | 14.7 |
| **25%** | 46.6 | 70.0 | 58.2 | 40.4 | 28.3 | 23.0 | 12.6 |
| **9.4%** | 30.0 | 46.8 | 39.0 | 28.3 | 20.8 | 16.8 | 9.8 |
| **3.1%** | 28.9 | 38.5 | 33.2 | 26.4 | 20.6 | 17.4 | 17.1 |

**The cut stops being the problem**: 0.3–11.4 ms across the whole grid, against 176 ms on the
ordinal-tree fixture. So the ridge that fixture showed was largely its own, and the downward walk's
guard — which measured worse when relaxed — was being blamed for it.

### What dominates instead: the masked count of a coarse node

| count only | 100% | 75% | 50% | 25% | 6.25% | 0.39% | 0.024% |
|---|---:|---:|---:|---:|---:|---:|---:|
| mask **100%** | 3.6 | 3.7 | 3.5 | 2.8 | 2.0 | 1.3 | 0.6 |
| mask **75%** | **52.3** | 52.0 | 49.2 | 37.8 | 26.5 | 22.8 | 12.6 |
| mask **50%** | 45.3 | 44.4 | 41.7 | 35.4 | 25.0 | 22.7 | 13.2 |

**Flat across the viewport and fourteenfold worse the moment the mask is not everything.** Both
follow from what it is: the count runs over the *served* set, which a budget bounds — but a shallow
cut serves **coarse** nodes, and a coarse node's membership is most of the corpus. `and_cardinality`
against a full mask is one run; against a fragmented one it is fifteen thousand containers, three
times over for base, minus and plus.

⊘ **Per-signature counts would remove it, and cheaply.** `|membership ∩ M_auth|` is
`Σ over satisfied signatures of |membership ∩ sig|` — build-time, mask-independent, and exact once
the overlay's small `minus`/`plus` are applied on top. It is only worth storing for the nodes where
the count is dear, which are the coarse ones: the top nine levels of a 10⁶-node tree are ~10⁴ nodes,
so ~1.3 MB at thirty-two signatures. Not measured, and its cost scales with the signature count
rather than the artifact count, which is the thing to check before building it.

## What this does not measure, stated so it is not read as settled

- **Not a real clustering.** The `runs` arm is a partition of the map into row-contiguous artifacts,
  which is what HDBSCAN over a Morton-ordered corpus gives; the `scattered` arm is an attribute
  predicate's carriers, which have no locality and which the index therefore cannot help. Real
  membership sits between, and the residency campaign puts it 14–170× cheaper per member than the
  synthetic arm.
- **One level, one layer.** A treed layer's serving loop runs the whole thing per level and a
  request may name several layers; both are linear multipliers on everything here.
- **The row-major arm is single-valued only.** An overlapping layer needs a list per row rather than
  a label — the same inversion at a larger constant, and the shape `artifacts-from-points` already
  calls a list column. Not measured.
- **Nothing is served.** The gather, the record-blob reads for supplied content, the wire encoding
  and the cut are all downstream of this and are not in these numbers.
