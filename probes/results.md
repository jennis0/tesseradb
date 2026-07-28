# Phase 0 measurements — results and analysis

All measurements taken against the corpus described in `dataset.md`.
The go/rework/stop verdict drawn from them is `phase0-memo.md`; this
document is the evidence. Environment throughout: WSL2, 12 cores, 39 GB,
RTX 3080. **Timings are indicative, not certified** — WSL2 was chosen
for mmap and page-cache fidelity, not latency. Treat ratios as evidence
and absolutes as a starting point.

---

## 1. Method, and two framing decisions

Every Phase 0 number is a function of two artifacts: the exploded
`(entity_id, term_id)` pair relation, and a grant set resolved to term
IDs. Generators exist only to pin the distributional knobs of those two
things. Both OPA-style and accumulo-access-style access control collapse
to that same seam, so the styles return later as conformance fixtures
against the `accumulo-access` oracle, not as measurement axes.

Two deliberate departures from plan §4.1:

**DNF expansion was not measured, because here it cannot be.** Our
predicates are our own knob, so measuring their expansion measures the
knob. Deep nesting is the term generator's problem and is resolvable by
minting synthetic terms for subexpressions, which keeps terms-per-item
linear in expression size at any nesting depth. Minting is a
conservation law, and both of its ends *were* measured: auth-side
(satisfied terms per token) swept directly to w=10⁴, item-side
(terms per item) to 1000 via `hiterms`.

**Grant sets are swept on a (width × coverage) grid** rather than
generated per policy style, since width is what actually reaches the
mask build.

## 2. Label-set profiles (measurement 1)

Base corpus, 2,422,486 items:

| Config | Pairs | Terms | Terms/item med/p99/max | >64 | Head posting | Signatures | Run len |
|---|---|---|---|---|---|---|---|
| categories-subclass | 4.16M | 176 | 1 / 5 / 13 | 0% | cs.LG 7.51% | 54,791 | 1.060 |
| categories-archive | 3.35M | 38 | 1 / 4 / 9 | 0% | **cs 26.59%** | 2,548 | 1.256 |
| surnames | 10.59M | 404,104 | 3 / 23 / 2,561 | **0.318%** | Wang 4.65% | 1,538,197 | 1.014 |
| hash-flat | 2.42M | 10,000 | 1 / 1 / 1 | 0% | 0.01% | 10,000 | **1.000** |
| hash-zipf1.4 | 2.42M | 31,614 | 1 / 1 / 1 | 0% | **32.48%** | 31,614 | 1.151 |
| hash-zipf1.2×64 | 2.42M | 361,837 | 1 / 1 / 1 | 0% | 0.31% | 361,837 | 1.001 |

- The only over-cap source is author-style labels — **0.318%** of items,
  the mega-collaborations, max 2,561 terms on one paper.
- The §3 head regime is reached two ways: topic-correlated (archive `cs`
  at 26.6%) and orthogonal (zipf1.4 at 32.5%).
- **Nothing runs under created-order entity IDs** (1.00–1.26 against a
  1.000 random baseline). §11.1's posting compression has to come from
  signature-sorted assignment within batches, not from arrival order.

## 3. Permission signatures (measurement 2)

Categories-subclass, swept over per-item noise ε:

| ε | Distinct signatures | Largest group |
|---|---|---|
| 0 | 54,791 | 83,863 |
| 0.01 | 66,463 | 83,091 |
| 0.1 | 118,732 | 75,908 |
| 0.3 | 182,363 | 62,359 |

**Collapse is gradual, not a cliff** — at ε=0.3 the mean group is still
~13, because a (1−ε)² share of items keeps its clean set. Author-like
policy sits at the opposite extreme (1.54M signatures over 2.42M items),
where aligned partitioning is unavailable. Top 500 groups cover 82.4% of
the corpus, top 1,000 cover 88.0%; group size by rank falls 4,213 (rank
100) → 444 (rank 500) → 158 (rank 1,000) → 42 (rank 2,500).

## 4. Mask build (measurement 3)

### 4.1 Two formulations, and a reassignment

Plan §4.2 names the DuckDB semi-join over the pair table as *the
authorise budget*. Measurement disagrees:

- **Semi-join**: 36 ms at 2.42M for w=10⁴, but 3.9–10.3 s at a tiled
  249.5M corpus — tripping the plan's own "multi-second" criterion at
  scale, tens of seconds extrapolated at 10⁹.
- **Postings union** (§6.3's serving shape): 4–540× faster, 36–530 ms at
  249.5M.

So the semi-join is **reassigned** to build-cadence machinery and the
differential oracle, with the postings union as the authorise path.
Probe-to-dictionary ratios are ≤0.03 in all realistic scenarios,
i.e. per-term-lookup territory under Druid's 0.12 heuristic.

### 4.2 At the true 10⁹

Postings-union path, min of 3, real vocabulary structure (superseding
the earlier tiled measurements, which replicated identical postings):

| Config | Scenario | w | Coverage | \|mask\| | union ms | ser MB |
|---|---|---|---|---|---|---|
| categories-subclass | head 25% | 9 | 26.4% | 264.3M | **230.6** | 125.12 |
| categories-subclass | random w=10⁴ | 10,000 | 41.6% | 416.1M | **588.0** | 125.12 |
| categories-archive | head 25% | 389 | 25.1% | 250.5M | **21.7** | 107.07 |
| categories-archive | random w=10⁴ | 10,000 | 72.0% | 720.4M | 337.8 | 125.12 |
| hash-flat | head 25% | 2,301 | 25.0% | 250.0M | **2,885.0** | 125.12 |
| hash-flat | random w=10⁴ | 10,000 | 100.0% | 1,000.0M | 13,783.5 | 125.12 |

**The extrapolation holds.** The realistic worst case — 10⁴ grants over
category-like labels — is **588 ms**, inside the 0.14–2 s band projected
from 250M. The authorise budget does not move.

**Entity-space contiguity is worth up to ~130×**, at identical coverage
and identical mask size. Union cost is Σ over granted terms of
*containers spanned*, not mask cardinality: hash-flat's terms hold ~100k
postings each spread over a 10⁹ universe, so each spans ~15,259
containers (all of them) at ~6.5 postings per container — 10⁴ terms ×
15,259 ≈ 152M container merges. categories-archive does *more* absolute
work in 337 ms because each replica-local term spans ~37 densely-filled
containers, 400× fewer visits. The same 100k postings cost ~15,259
container-visits scattered and ~2 contiguous.

*(The 13.8 s row is degenerate — that principal sees 100% of the corpus,
so there is nothing to mask. Compare equal-coverage rows.)*

**Mask sizes land exactly on Appendix A**: every ≥25%-coverage mask
serialises to 125.12 MB, the dense bound for a 10⁹ universe.

**Implication for the kernel (Phase 2):** for many small scattered
terms an n-way Roaring union is the wrong algorithm. Concatenating
sorted posting arrays, radix sorting and bulk-constructing is O(total
postings) rather than O(terms × containers) — the construction §10.4
already prescribes for the permutation. The kernel wants a heuristic on
postings-per-container spanned, in the spirit of Druid's 0.12 ratio but
on a different quantity.

### 4.3 Dictionary scale — surnames fold sweep at 10⁹

One file, two vocabularies via the fold knob, holding pairs, coverage
and spatial footprint constant. 4,369,612,182 pairs throughout.

| Fold | Distinct terms | Posting med/p99/max | Singletons | head 25% | random w=10⁴ |
|---|---|---|---|---|---|
| 1 | **116,902,007** | 3 / 348 / 31.9M | 40.2M (34.4%) | 552 ms | 93.5 ms |
| 12 | 10,021,684 | 36 / 4,824 / 31.9M | 0 | 466 ms | 240.6 ms |

- **Dictionary scale is a storage problem, not an authorise-path
  problem.** Mask build barely moves between 10M and 117M terms, because
  union cost depends on granted postings, not vocabulary size. Two
  orders past the design's assumed 10⁵–10⁶ terms, the budget is
  unaffected.
- **Storage holds**: 17.5 GB CSR postings + 0.94 GB offsets at 117M
  terms, inside Appendix A's ~20 GB term-index budget at 10⁹. As a
  Roaring bitmap per term it would be ~29 GB of overhead alone.
- **34.4% of terms are singletons** — the extreme of §6.2's small-term
  regime, which makes "sorted int32 arrays below a few hundred members"
  load-bearing rather than an optimisation. ClickHouse's threshold of 32
  is the calibration point.

### 4.3b Scaling curves, with container counts

Postings built once per config at 10⁹ then range-restricted per scale,
so the curve isolates scale. `hash-flat`, `random w=100` (~1% coverage
at every scale) is the clean fixed-*w* row:

| Scale | \|mask\| | Containers | union ms | ns/container |
|---|---|---|---|---|
| 250,000 | 2,510 | 399 | 0.0 | — |
| 2,422,486 | 24,120 | 3,693 | 0.2 | 54 |
| 250,000,000 | 2,511,169 | 380,868 | 36.3 | 95 |
| 1,000,000,000 | 9,931,985 | 1,523,216 | 173.6 | 114 |

**Union cost is linear in containers spanned**, with per-container cost
drifting up only ~2× across 400× of scale (memory hierarchy). Container
count tracks N exactly at fixed coverage, and saturates at 15,259 —
10⁹/2¹⁶, every container in the universe — which is why a single 123k-
posting term at 10⁹ already spans all of them.

**But there are two regimes, and the per-container constant differs by
13×.** categories-subclass at 10⁹, head 25%: 125,279 containers,
193.1 ms = **1.54 µs/container**, against hash-flat's 114 ns. The cause
is container *type*: categories' terms hold ~1,900 postings per
container (dense → **bitmap** containers, a fixed ~8 KB pass per merge,
memory-bandwidth bound at ~5 GB/s), hash-flat's hold ~6.5 (sparse →
**array** containers, tiny merges dominated by per-container dispatch).

So the refined model is *Σ over container merges of work per merge*,
where a bitmap merge is a fixed 8 KB pass and an array merge is
proportional to its elements. This sharpens §4.2's two-algorithm
recommendation: **concatenate-and-sort wins in the sparse-array regime**
(Roaring pays dispatch overhead for a handful of elements) and **Roaring
union wins in the dense-bitmap regime**. The crossover is essentially
Roaring's own array/bitmap threshold, which is a more principled
heuristic than postings-per-container alone.

*Caveat on the head rows:* head grants select terms until a coverage
target, so `w` changes with scale (2 → 4 → 9 for categories). Those rows
conflate scale with grant composition; the fixed-*w* rows above are the
clean sweep.

### 4.4 Signature-sorted entity assignment, simulated

§11.1 prescribes assigning entity IDs sorted by term signature within
each batch. Simulated as a global re-permutation, with postings and a
25%-coverage fragment rebuilt under both orderings:

| Config | Postings total | 25% fragment |
|---|---|---|
| categories-subclass | 7.7 → 0.9 MB (**8.9×**) | 303 → 47 KB (**6×**) |
| surnames | 44.8 → 45.5 MB (1.0×) | 303 → 303 KB (1×) |
| hash t=100 | 552.9 → 15.1 MB (**36.7×**) | 303 KB → **0.2 KB** (**1,614×**) |

These are **floors on the scale effect**: at 2.4M the mean categories
group is 44 entities, so runs are short; at 10⁹ groups are ~400× longer
and index size converges on O(Σ|signature| × batches) — a function of
policy complexity, not corpus size. Entity space only; the row-space
mask stays scattered (§5). Together with §4.2's 130× union result, this
is the same mechanism measured from two directions.

## 5. Spatial autocorrelation under Morton order

The §16 open question. Run ratio = mean run length of the mask in row
order ÷ the 1/(1−p) expectation for a random mask of equal density.

| Config | Scenario | Coverage | Run ratio | d6 tiles <5% | d6 median cov |
|---|---|---|---|---|---|
| hash-flat | all | 0.01–25% | **1.00** | (validates estimator) | = coverage |
| surnames | head 25% | 25.0% | 1.15 | 1.8% | 20.0% |
| surnames | head 4.6% | 4.6% | 1.05 | 63.3% | 3.6% |
| surnames | random w=100 | 0.13% | 1.03 | 98.8% | 0.13% |
| categories-subclass | head 25% | 25.4% | 2.32 | 30.8% | 17.3% |
| categories-archive | head 25% | 26.6% | **5.11** | 38.9% | 16.4% |

1. **The flat-hash control returns exactly 1.00**, so the estimator and
   pipeline are sound and every other ratio is interpretable.
2. **Realistic masks are essentially scattered.** Surnames — the most
   realistic principal shape available — sits at 1.03–1.15. Categories
   reach only 1.7–2.3, and the archive head 5.11; those are the family
   the topic-correlation caveat says is *flattered*, so they are upper
   bounds.
3. **Morton order buys no Roaring compression** for any family: entity-
   and row-space serialisations are within noise.
4. **The scaling analysis's uniform-scatter assumption — flagged as its
   weakest and conservative — is measured ≈true.** Its residency and
   paging figures are forecasts, not floors. There is no hidden upside.
5. **Direct evaluation is the main selection path, with duty-cycle
   numbers**: at working coverages 12–99% of occupied depth-6 tiles fall
   below the ~5% crossover; for tail-only principals essentially all do.
   Candidate lists serve only the dense cores of head principals.
6. **The topic-correlation caveat is quantified**: category-derived
   terms overstate clustering by up to ~5× relative to orthogonal
   structure.

## 6. The core primitive at 10⁹

`range_cardinality` over real Morton tiles, min of 5 (a single-shot
first pass reported figures 5–10× higher and was noise — repeat before
believing). Mask: 6 global terms → 69.3M items, 6.93% coverage.

| Depth | Tiles | Rows/tile | Visible | Total | µs/tile |
|---|---|---|---|---|---|
| 0 | 1 | 1,000,000,000 | 69,329,737 | **0.19 ms** | 190.9 |
| 2 | 16 | 62,500,000 | 69,329,737 | **0.10 ms** | 6.3 |
| 4 | 256 | 3,906,250 | 69,329,737 | **0.31 ms** | 1.2 |
| 6 | 300 | 266,845 | 6,448,061 | 0.31 ms | 1.0 |
| 8 | 300 | 22,612 | 584,004 | 0.26 ms | 0.9 |
| 10 | 300 | 9,597 | 288,780 | 0.25 ms | 0.8 |
| 12 | 300 | 8,637 | 268,300 | 0.24 ms | 0.8 |

**Whole-viewport exact masked counting costs 0.1–0.3 ms at every zoom
level**, including depth 0 where one call counts the entire billion rows
in 191 µs. Cost is O(containers spanned) and the mask has only ~15,000
containers, so any set of ranges is bounded by one full traversal — it
cannot blow up at coarse zoom. This **corrects a worry in §13.3**: the
zoomed-out overview is expensive because of mask materialisation and
shard fan-out, not because of counting.

The **permutation into row space is the expensive step** — 8.8 s for a
69M-item mask. That is exactly why §10.4 caches it per *(token, slice,
pin)*, and the number makes the consequence concrete: if it ever drifts
onto the per-viewport path, the system dies.

## 7. Per-item breadth (`hiterms`)

Three variants at 10⁷ entities, terms/item **min 10, median 100, p99
955, max 1000** — **59.4% of items over the 64-term threshold** by
construction. Overlap varies how much of an item's term set comes from
its profile band.

| Config | Pairs | Terms/item | head term coverage | random w=10⁴ union |
|---|---|---|---|---|
| hiterms (overlap 0) | 1.299B | 129.9 | 96.97% (w=1) | 236.6 ms |
| hiterms-ov0.5 | 1.384B | 138.4 | 90.59% (w=1) | 301.1 ms |
| hiterms-ov0.9 | 1.249B | 124.9 | 64.52% (w=1) | 297.1 ms |

Masks cap at 1.25 MB — the dense bound for a 10⁷ universe. Postings
build (the index-build step, batch work) took 219–278 s for ~1.3B pairs.

**What this says about the per-item cap** (memo action 4):

- **Mask build is unaffected in kind.** 237–301 ms for 10⁴ grants over
  10⁷ entities carrying ~130 terms each. Nothing about high breadth
  reaches the authorise path as a new cost *shape* — it is the same
  container-bound union as everywhere else.
- **The cost is the pair relation, and it is linear.** ~130 pairs per
  item is 1.3B pairs at 10⁷ entities (1.76–2.71 GB encoded); at 10⁹
  entities the same breadth would be ~1.3×10¹¹ pairs, on the order of
  200 GB. That is the "space inefficiency" the proposal accepts, and it
  is why these configs are entity-capped.
- **One term covering ~97% of the corpus is an artefact of breadth, not
  of policy.** With 130 Zipf draws per item, almost every item picks the
  head term. It is worth noting because it makes `head c~0.25` reach
  25% coverage at w=1, which flatters that row.
- Higher overlap lowers head-term coverage (97% → 65%) without changing
  union cost materially, so profile structure does not itself introduce
  a new cost.

### 7.1 §7.6's assumption — terms per node

§7.6 rests on "a typical node draws on a modest number of terms",
flagged unmeasured in §16. Morton tiles stand in for cluster nodes
(both are spatially coherent item groups). At depth 8, median 47 items
per tile, 150 tiles sampled, 10⁷ entities:

| Config | Terms/tile (median) | p95 | Terms per item |
|---|---|---|---|
| categories-subclass | **14** | 37 | 0.26 |
| hiterms (~130 terms/item) | **3,928** | 26,621 | 72.7 |
| hiterms-ov0.9 | **5,832** | 55,816 | 116.4 |

**With ordinary labels the assumption holds comfortably** — a 47-item
node draws on 14 distinct terms. **At high breadth it breaks
decisively**, by 280–420×.

The consequence is labelling cost, not safety: containment stays exact,
but a labeller enumerating single-term generating sets faces ~4,000–6,000
candidates per node instead of ~14, the `/control/nodes/{id}/term-
distribution` endpoint returns that many entries, and §7.8's "nested
chain ordered by term mass, plus individual single-term sets" becomes a
candidate set the caller must **cap by mass** rather than enumerate.
That is a caller-pipeline consequence to record against §7.8, and it
does not block the per-item-cap proposal (memo action 4), which turns on
safety and availability rather than labelling cost.

**Caveat on the overlap knob.** Overlap 0.9 produced *more* terms per
tile, not fewer, because profiles are assigned randomly per item — so
spatially co-located items belong to different profiles and share less
than items drawing from one global Zipf. The knob models "items share
term sets" but not "a cluster shares term sets"; real policy structure
would likely correlate with content and therefore position. These
numbers bound the uncorrelated case, which is the pessimistic one.

## 8. Cross-cutting

Two mechanisms account for most of what was measured:

**Container footprint, not cardinality, is the cost model** for every
bitmap operation here. It explains the 130× union spread, the 8.9–36.7×
posting compression from signature ordering, the flatness of viewport
counting across zoom, and why dictionary scale is free on the authorise
path. Where the design already reasons this way (§10.4's batched select,
§11.1's ordering) it is right; where an implementation forgets it, that
is where the 100× regressions will be.

**Almost every headline is policy-dependent.** Signature alignment,
posting compression and union speed all depend on the shape of the
access labels, not on corpus size — the same histogram decides all
three, and author-like policies get none of them while category-like
policies get all three. This is the strongest argument for the real-label
rerun retaining go/rework/stop authority: the *machinery* is validated,
the *deployment's numbers* are not.
