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

## What this does not measure, stated so it is not read as settled

- **Not a real clustering.** The `runs` arm is a partition of the map into row-contiguous artifacts,
  which is what HDBSCAN over a Morton-ordered corpus gives; the `scattered` arm is an attribute
  predicate's carriers, which have no locality and which the index therefore cannot help. Real
  membership sits between, and the residency campaign puts it 14–170× cheaper per member than the
  synthetic arm.
- **One level, one layer.** A treed layer's serving loop runs the whole thing per level and a
  request may name several layers; both are linear multipliers on everything here.
- **Nothing is served.** The gather, the record-blob reads for supplied content, the wire encoding
  and the cut are all downstream of this and are not in these numbers.
