# Where a filter's values live — the render column, and a list column's addressing

**Date:** 2026-08-12 · **Harness:** [`placement/`](placement/) · **Machine:** WSL2, 12 cores, 47 GB
RAM, **single-threaded** · **Raw:** [`run-viewport.csv`](run-viewport.csv),
[`run-coarse.csv`](run-coarse.csv), [`run-lists.csv`](run-lists.csv)

Two questions, both about **placement** rather than about the scan: can a filter be answered from the
**render column** a rendered attribute already stores in row space, instead of from the entity-space
copy beside it — and what addressing does a **multi-valued** column need, `filter-index.md` §2.1's
one-presence-bit-one-slot rule being what a list breaks.

Every arm checks its routes against each other before reporting a timing: a faster route that answers
differently is not a faster route. Arm 1's routes agreed on all 48 cells, arm 2's on all 16, arm 3's
on all 216 — after a defect in the probe's own pairs layout, which counted matching *pairs* rather
than matched *needles* and so read an entity carrying a value twice as satisfying `all_of`.

## Results

### 1. A filtered viewport answered from the render column costs the viewport, not the corpus

**7× to 1,269× at 10⁸ over a 300,000-row viewport, and 16× to 10,038× over a 30,000-row one.** The
entity-space route's cost is set by the principal — it scans the whole authorised set before anything
about the request narrows it — while the render column's is set by what is on screen.

| mask | coverage | E: scan | E: cross | E: total | R-dense | R-masked | E ÷ best R |
|---|---|---|---|---|---|---|---|
| contiguous | 1% | 0.269 | 0.115 | **0.384** | 0.180 | 0.051 | 7.5× |
| contiguous | 25% | 6.318 | 1.281 | **7.599** | 0.188 | 0.335 | 40× |
| blocked | 1% | 0.272 | 0.102 | **0.374** | 0.219 | 0.051 | 7.3× |
| blocked | 25% | 6.396 | 1.394 | **7.790** | 0.210 | 0.313 | 37× |
| scattered | 1% | 9.327 | 0.180 | **9.507** | 0.198 | 0.062 | 153× |
| scattered | 25% | 219.397 | 1.455 | **220.852** | 0.174 | 0.320 | **1,269×** |

*ms at 10⁸, 300,000-row viewport (300 tiles × 1,000 rows), a category matching 0.1% of entities.*

The constants behind it, and the first is a check on the harness rather than a finding: **the shipped
`ValueColumn::scan_eq` measures 0.25–0.27 ns per candidate entity on a contiguous or blocked
candidate and 8.6–9.8 ns on a scattered one**, reproducing `filter-index.md` §2.2's published ~0.28 /
~9.6 on a different fixture. Against it:

- **R-dense — 0.48–0.73 ns per viewport row**, and *invariant* in corpus size, mask shape and
  coverage. It reads the same bytes whatever the principal may see, so the work carries no dependence
  on the mask at all.
- **R-masked — 3.4–20.7 ns per *masked* row.** Fewer rows read, each read scattered: a row-space mask
  is the *projection* of an entity-space one, so it has no run structure however contiguous `M_auth`
  was. It wins below roughly 10% coverage and loses above it, and the two variants bracket the route.

**The deployed gap is at least the measured one.** Everything here is single-threaded, but the shipped
filter evaluation is serial by construction — no rayon in `tessera-filter` or in the engine's
`filter.rs`, and it runs on the request thread before the tile sweep — while a row-space scan would
sit *inside* the sweep that is already parallel over tiles.

### 2. At the coarsest zoom the two swap places, and neither dominates

A row-space route costs O(rows in view), and at zoom 0 the view is the corpus. This is the cell that
decides whether the entity-space copy can be dropped outright or only bypassed:

| mask | coverage | predicate | E: total | R-dense | R-masked | winner |
|---|---|---|---|---|---|---|
| blocked | 1% | 0.1% | **0.58** | 43.45 | 17.16 | E, 30× |
| blocked | 25% | 0.1% | **11.93** | 42.31 | 81.90 | E, 3.5× |
| blocked | 25% | 25% | 153.92 | 258.22 | **80.76** | R, 1.9× |
| scattered | 1% | 0.1% | 9.17 | 42.21 | **16.92** | E, 1.8× |
| scattered | 25% | 0.1% | 206.55 | **42.32** | 80.37 | R, 4.9× |
| scattered | 25% | 25% | 434.41 | 267.06 | **82.08** | R, 5.3× |

*ms at 10⁸: 4,096 tile counts over the whole slice. The 10⁹ figures below are ten times these —
**modelled, not measured**, and the ×10 is the routes' shape rather than a measured linearity. Over
10⁷→10⁸ the per-decade factor across these eighteen cells runs **4.83× to 30.2×**: tightest on
R-dense (8.69–10.22×, the route whose domain is the corpus by construction), 4.83–12.92× on E-total,
and loosest where a fixed setup cost dominates the 10⁷ cell rather than the work does — R-masked at
1% coverage measures 16.8× and 30.2×, from 1.02 ms and 0.56 ms bases. Read the ×10 as the asymptote
these approach from either side, not as a fit.*

The entity route wins where its scan is cheap and its result small — a contiguous mask, low coverage,
a selective predicate. The render route wins where the entity route's two corpus-scale terms bite: a
scattered principal (its scan is 35× dearer per candidate) or a broad predicate (its projection costs
144–151 ms of the total). **At 10⁹ both are at or over `filter-index.md` §2.2's 0.5–1 s filter
budget in their bad cells** — 0.4–2.6 s modelled for the render route, 0.006–4.3 s for the entity one —
so this is a cell that needs the parallelism neither arm used, whichever route serves it.

### 3. A list column: postings win every timing cell but one, and the storage ranking inverts

The two shapes are decision 0039's own corpus measurements — `categories` (176 distinct, mean 1.72)
and `surnames` (404,104 distinct, mean 4.54, 138,861 singletons) — modelled as (mean 2, domain 200)
and (mean 5, domain 400,000) with a quadratically skewed vocabulary.

**ns per candidate entity, `any_of` over three values, 10⁸:**

| | contiguous 1% | blocked 25% | scattered 1% | scattered 25% |
|---|---|---|---|---|
| **CSR**, mean 2 / 200 | 8.7 | 8.0 | 49.1 | 13.5 |
| **postings**, mean 2 / 200 | **10.08** | **0.35** | **9.74** | **0.67** |
| **pairs**, mean 2 / 200 | 10.6 | 9.6 | 346.4 | 54.9 |
| **CSR**, mean 5 / 400k | 10.8 | 10.3 | 87.2 | 17.1 |
| **postings**, mean 5 / 400k | **6.24** | **0.21** | **6.26** | **0.30** |
| **pairs**, mean 5 / 400k | 13.5 | 12.8 | 483.9 | 71.4 |

**Against CSR the postings span 0.86× to 57.0×**, paired cell by cell, and the low end is a real
loss rather than a rounding: on a contiguous 1% candidate at the small dense vocabulary CSR costs
8.7 ns against the postings' 10.08. That is the one losing cell of eight; the other seven run
1.73× (contiguous 1%, large sparse vocabulary) to 57.0× (scattered 25%, same). "10–40×" describes
neither end.

Constants hold from 10⁷ to 10⁸. `all_of` and `none_of` rank identically; CSR's `none_of` costs
~1.4× its `any_of` (every value must be examined, none can short-circuit), and postings answer
`all_of` fastest of all — 0.01–0.5 ns/candidate, an intersection of two bitmaps.

**Bytes per entity, 10⁸:**

| | CSR (u32 values) | postings | pairs |
|---|---|---|---|
| mean 2, domain 200 | 11.9 | **3.8** | 15.9 |
| mean 5, domain 400,000 | **24.0** | 32.3 | 40.0 |

**The ranking inverts, and the inversion is the finding.** A small dense vocabulary makes a posting
per value both the smallest and the fastest structure — one bitmap replaces millions of repeated
codes, exactly `filter-index.md` §2.3's argument for the single-valued category accelerator. A large
sparse one makes it the *largest*: 400,000 bitmaps whose tail is singletons pay per-bitmap overhead
against no repetition to amortise it. CSR's cost is the opposite way round — flat 4 B/entity of
offsets plus the values themselves, indifferent to the vocabulary.

**Explicit `(entity, value)` pairs are never optimal on either axis**, at any shape measured —
the same verdict `probes/2026-08-08-filter-layout/` reached for the single-valued column, and for a
sharper reason here: a scattered candidate makes it 346–484 ns per candidate entity, a binary search
per run against a walk of adjacent bytes.

### 4. What the second copy costs, as arithmetic

An entity-space value column is the declared width per entity, so at 10⁹ it is 1 GB for a `u8`
category, 2 GB for a `u16`, 4 GB for an `f32` or `u32`, 8 GB for an `i64` or a `timestamp_us` —
**per column**, against Appendix A's ~20 GB term-index budget and beside a row-space copy of the same
values that already exists. A rendered column declared filterable today pays both.

## What to build

- **Serve a filtered viewport from the render column**, where the column is rendered. It is 7×–1,269×
  cheaper at 10⁸ on the request shape a viewer actually issues, it needs no entity-space artefact, and
  it needs nothing on the write side: a flush, a merge and the fold already carry the scalar tail, so a
  render column is current in row space by construction — no extents, no coalesce axis, no fold pass,
  no postings rebuild.
- **Keep the mask-first and dense variants both**, choosing by coverage against the viewport's rows.
  The choice is a function of the request and the principal's own cardinality, exactly as
  `filter-surface.md` §4's existing crossover rule is.
- **Do not drop the entity-space column on the strength of arm 1.** Arm 2 is the counter-case, and it
  is not a corner: a coarse-zoom filtered count over a contiguous low-coverage principal is 30×
  cheaper from the entity-space column.
- **A list column is a CSR flat column** — values in entity order, `offsets[e]..offsets[e+1]`
  addressing them — **with the per-value postings a category already derives.** That is the
  single-valued design's own rule (the flat column is the record, every accelerator is derived from
  it) extended without amendment, and the measurements support both halves: CSR alone is 8–14 ns per
  candidate entity contiguous, which is the *string* column's budget class rather than the fixed-width
  one, and postings recover **up to 57× where a vocabulary exists to key them on — in seven of the
  eight cells, losing the eighth at 0.86×** (contiguous 1%, small dense vocabulary).

## What this does not settle

- **Nothing here is parallel.** Both routes parallelise over their own axis and the coarse cell needs
  it, so every 10⁹ figure above is a single-threaded extrapolation of a single-threaded measurement.
- **The row-space route was written for this probe.** There is no shipped row-space filter to call, so
  arm 1's R constants describe the approach, not code — unlike its E constants, which are
  `ValueColumn::scan_eq` itself. Expect the built version to be slower by whatever the segment
  boundary and the `ScalarSlice` match cost, neither of which is in these numbers.
- **One segment, one slice.** The fixture is a single base segment; a real slice is a base plus flushed
  segments, so the render route's walk is per segment and its per-range setup is paid more often.
- **A skewed vocabulary is modelled, not sampled.** Arm 3's tail is `D·u²` rather than the arXiv
  distribution itself; the postings storage figure for a large vocabulary is the number most likely to
  move if it were sampled properly.
- **Presence is universal in every arm.** A partial column adds a Roaring presence bitmap and the
  affine-rank traversal to the entity-space routes, and adds nothing to the render one — which, being
  row-space and non-nullable, has no absence to address and cannot distinguish an absent value from
  the type's zero (decision 0064's render half, still open).
