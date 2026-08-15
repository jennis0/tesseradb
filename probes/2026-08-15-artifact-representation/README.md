# Artifact membership — where it lives, and what that costs

**Result: membership belongs in row space.** Row-space bitmaps are **28–118× smaller than
entity-space** on real membership and **5× smaller than an assignment column** at 10⁹ — that is the
campaign's solid finding. **The cost comparison is a crossover, not a win:** per-artifact testing
beats a column scan for coarse levels and loses to it for fine ones.

> **Three harness bugs were found in review round two (2026-08-15), all reproduced before
> correcting, and one reverses a conclusion.**
> **(a)** The generator laid artifacts out in Zipf-rank order along the row axis, so narrow viewports
> held almost no artifacts — M4 re-run, and the column now **wins** at fine levels.
> **(b)** *"~80 bytes per artifact, flat"* was an artifact of holding `rows/artifacts = 100` at every
> scale point; the scale-free quantity is **per member**.
> **(c)** M8's *"56× for corridors and archipelagos"* was the probe's own subsampler, not the shapes —
> corrected figures are 1.1–4.6×.
> All three were found by reviewers, none was visible from the prose, and the corrected numbers are
> below.
Two further results: the signature-sort tiebreak buys **4.08×** on the disk form for **no** posting
cost, and resolving a whole layer's visibility costs about **1.4× a mask build**, which dissolves
the artifact drill-down leak-register row the design was about to escalate.

Re-run:

```
python3 probes/2026-08-15-artifact-representation/tier_a_cluster.py 250000   # ~8 min
python3 probes/2026-08-15-artifact-representation/m1_spaces.py
python3 probes/2026-08-15-artifact-representation/m2_representations.py
python3 probes/2026-08-15-artifact-representation/m3_sort_order.py           # ~15 min
cargo run --release --example artifact_layers -p tessera-store -- \
    --rows 1000000000 --artifacts 10000000 --reps 2
```

```
python3 probes/2026-08-15-artifact-representation/m8_geo_shapes.py
```

Raw Rust output in `1e7-1e5.txt`, `1e8-1e6.txt`, `1e9-1e5.txt`, `1e9-1e7.txt`.

---

## The two tiers, and what each is for

**Tier A is real all the way down.** 2,422,486 arXiv papers, BGE embeddings, PCA to 64 components,
cuML UMAP, quantised to the 2¹⁶ grid (`probes/dataset.md`). The built bundle stores that geometry as
a sorted `morton.u32` per row, which **decodes back to the real projected coordinates** — row order
*is* Morton order — and carries the real `row-entity` permutation and the real category/author term
structure beside it. `sklearn.cluster.HDBSCAN` was run over those coordinates at three
`min_cluster_size` settings, and every entity-space figure below uses the permutation the shipped
build actually produced.

| level | `min_cluster_size` | clusters | noise |
|---|---:|---:|---:|
| L0 | 6 000 | 8 | 20.2% |
| L1 | 600 | 111 | 23.3% |
| L2 | 60 | 884 | 24.7% |

HDBSCAN ran on a 250 000-point sample (2.4M is not tractable here) with every row then assigned by
nearest centroid and the measured noise fraction restored by a distance cut. **The sample bounds
what Tier A licenses**: the cluster *count* and *noise fraction* are HDBSCAN's, the fine detail of
cluster boundaries is not.

**Tier B is synthetic and exists only to reach the scales that matter.** Real HDBSCAN at 10⁹ is not
happening. The generator draws Zipf-distributed, Morton-contiguous membership at Tier A's measured
22% noise — the only property either representation is sensitive to — with no bundle on disk, the
approach `probes/2026-08-14-project-decomposition/` used to reach 10⁹ in memory. Disk, not memory,
is the binding constraint here: 16 GB free against 47 GB of RAM.

---

## M1 — row space against entity space, on real membership

**The same membership, the same library, the same corpus. Only the id space differs.**

| level | members | row space | entity space | ratio |
|---|---:|---:|---:|---:|
| L0 | 1 934 209 | **0.011 MB** | 1.279 MB | **118.5×** |
| L1 | 1 857 630 | **0.042 MB** | 3.267 MB | **77.9×** |
| L2 | 1 824 800 | **0.132 MB** | 3.801 MB | **28.7×** |

Row space is Morton rank, so a spatially coherent cluster is a handful of contiguous runs; entity
space is term-signature order, which is uncorrelated with position, so the same set scatters across
every container. **The coarser the level the larger the win**, because a coarse cluster is a longer
run.

`run_optimize` changes nothing in either space (1.00×) — croaring's portable serialiser already
picks the right container — so the ratio is a property of the data, not of an encoding flag.

## M2 — four representations, same membership

Real corpus, all three levels, ordinal 0 reserved for *no artifact*:

| | L0 | L1 | L2 | total |
|---|---:|---:|---:|---:|
| dense column | 2.422 | 2.422 | 4.845 | **9.69 MB** |
| partial-presence column | 1.939 | 1.875 | 3.725 | 7.54 MB |
| entity-space bitmaps | 1.279 | 3.267 | 3.801 | 8.35 MB |
| **row-space bitmaps** | **0.011** | **0.042** | **0.132** | **0.185 MB** |

**Row-space bitmaps are 52× smaller than the dense column** over the real corpus.

### The ratio is not scale-invariant, but the per-artifact cost is

Tier B, with the artifact counts the design assumes:

| rows | artifacts | row bitmaps | dense column | ratio | **B/artifact** |
|---:|---:|---:|---:|---:|---:|
| 10⁷ | 10⁵ | 8.04 MB | 40 MB (u32) | 4.97× | 80.4 |
| 10⁸ | 10⁶ | 79.8 MB | 400 MB (u32) | 5.01× | 79.8 |
| 10⁹ | 10⁷ | 794 MB | 4 000 MB (u32) | 5.04× | 79.4 |
| 10⁹ | 10⁵ | 477 MB | 4 000 MB (u32) | 8.38× | 4 771 |

### The per-artifact rule was wrong — the scale-free quantity is per member

An earlier revision read the first three rows as *"~80 bytes per artifact, flat across two orders of
magnitude"* and built the formula `(rows × width) / (80 × artifacts)` on it. **All three rows hold
`rows/artifacts = 100`.** Off that ray the rule collapses, and this campaign's own fourth row already
showed it:

| rows | artifacts | rows/artifact | B/artifact | **B/member** |
|---:|---:|---:|---:|---:|
| 10⁷ | 10⁵ | 100 | 80.4 | 1.03 |
| 10⁸ | 10⁶ | 100 | 79.8 | 1.02 |
| 10⁹ | 10⁷ | 100 | 79.4 | 1.02 |
| 10⁸ | 10⁵ | **1 000** | **631.4** | 0.81 |
| 10⁹ | 10⁵ | **10 000** | **4 771** | 0.61 |

The 10⁸/10⁵ row is new, run to test the rule off-ray, and it refutes it: **631 B/artifact.** The
formula predicted 50× against the column there and 500× at 10⁹/10⁵; the measurements are **6.33×** and
**8.38×**. Applied to a coarse level over a large corpus the discarded rule underestimated storage by
about 60×.

**The causal story was also false of this generator.** Runs do not stay fixed as a span grows —
`assign()` scatters 22% noise uniformly through each artifact's contiguous span, so a run breaks at
roughly every noise point and run count grows with span.

**Size per member, not per artifact:**

| Anchor | B/member | What it is |
|---|---:|---|
| Tier A, **real** HDBSCAN membership | 0.006–0.073 | real clusters, peripheral noise |
| Tier B, synthetic | 0.61–1.03 | uniform interstitial noise — **pessimistic** |
| Roaring array container | 2.0 | the arithmetic ceiling |

**Size from ~1 B/member and treat it as conservative.** Real membership measured 14–170× better than
the synthetic arm, because where the noise sits — not how much of it there is — drives the run count,
and Tier B places it adversarially.

## M4 — the count, both routes: **re-run, and the conclusion reversed**

> **The first run measured the generator's layout, not a clustering.** It laid artifacts down in
> Zipf-rank order along the row axis, so the giants came first and every narrow viewport fell inside
> one — 12 artifacts in range at a 1% window over 10⁷. Checked against this campaign's own **real**
> Tier A assignment, a window holds **1.1–1.7× (window fraction × artifact count)**: at L2, 14.8 of
> 884 clusters at 1%, where the generator implied a handful. The generator now shuffles span
> assignment; every figure below is from the corrected harness.

Per-artifact `and_cardinality` against a 25% scattered grant, versus one pass over visible rows
accumulating into per-ordinal counters. Both routes return identical sums at every setting.

**10⁹ rows, 10⁷ artifacts — a fine level:**

| viewport | visible rows | artifacts in range | per-artifact | column scan | winner |
|---|---:|---:|---:|---:|---|
| 100% | 250 000 378 | 10 000 000 | 5 852 ms | **1 849 ms** | column, 3.2× |
| 10% | 25 001 497 | 994 695 | 746 ms | **193 ms** | column, 3.9× |
| 1% | 2 500 545 | 102 870 | 72.8 ms | **37.6 ms** | column, 1.9× |

**10⁹ rows, 10⁵ artifacts — a coarse level:**

| viewport | artifacts in range | per-artifact | column scan | winner |
|---|---:|---:|---:|---|
| 100% | 100 000 | **309 ms** | 1 361 ms | bitmaps, 4.4× |
| 10% | 10 773 | **33.6 ms** | 105 ms | bitmaps, 3.1× |
| 1% | 1 070 | **4.1 ms** | 13.2 ms | bitmaps, 3.2× |

**So the cost conclusion crosses over.** *(Owner ruling, 2026-08-15: one implementation regardless —
row-space bitmaps. The crossover sits near 19 000 artifacts in range at a 1% viewport, which is past
what any client renders, so the column's regime is one the system declines to serve rather than one it
optimises for. Design §2.0.0 carries the argument and names the accepted 2–4× cost.)* Per-artifact
cost tracks *artifacts in range*; column cost tracks *visible rows*. Whichever quantity is smaller
wins, and at 10⁷ artifacts a viewport holds enough of them that the column takes it. The earlier
claims — *"faster at every viewport size"*, *"120×, structural rather than a constant a better column
implementation recovers"* — are **refuted by the corrected harness**, and refuted in the direction
that matters: they were the argument for deleting the column.

**Two caveats that both push the same way.** The column route here indexes by row directly, where a
real entity-indexed column needs a `row-entity` lookup first — so the column arm is **optimistic**.
And `in_range` is computed outside the timed region, so the per-artifact arm assumes a candidacy
structure over 10⁷ extents that this campaign never built or priced — so that arm is optimistic too.
The gap is close enough that neither can be waved away.

**What survives unchanged:** the **size** results — 5.04× against the column at 10⁹, and Tier A's
28–118× row-space against entity-space on real membership — and **M6**, which is the 100% row and is
layout-independent.

## M3 — the signature-sort tiebreak

Entity ids are allocated `(signature, source_id)`; the minor key is what
`architecture.md` §11.1 measures as worthless (run lengths 1.00–1.26 against a 1.000 random
baseline). Re-deriving the allocation as `(signature, morton)` over the real corpus, with the same
signatures and the same real term structure:

| | `(signature, source_id)` | `(signature, morton)` | |
|---|---:|---:|---|
| artifact membership | 8.893 MB | **2.181 MB** | **4.08× better** |
| term postings | 0.557 MB | 0.557 MB | **1.00× — unchanged** |

**The posting win is untouched, byte for byte**, which is what the argument predicted: a term's
postings are the union of the signature groups carrying it, and each group stays a contiguous run
whatever orders its interior. The minor key was free and is now spent.

**But M1 reframes what it is worth.** 4.08× applies to the *entity-space* form, which M1 shows is
28–118× worse than row space to begin with. With the hot path in row space this is a **disk-form and
projection-input** optimisation, not a request-path one. It remains free, and it remains permanent
under **I9** — it cannot be retrofitted, so it is decided before the first build that writes
artifacts or not at all.

## M6 — lazy family visibility resolution

Resolving *every* artifact's threshold for a session is M4's 100% row: **883 ms** at 10⁷ artifacts,
245 ms at 10⁵. Against the corpus's *measured* 588 ms realistic-worst-case mask build, that is
**~1.5× one authorise-time step a session already pays**, once per family, cacheable for the
session's life and invalidated on the same generation key as everything else.

**This dissolves the escalation the design was carrying.** `annotation-representation.md` §8 argued
that artifact drill-down defeats the structural closure Appendix C's C4 annotation gives
`/v1/items/{tessera_id}` — an artifact under the derived gate needs an `and_cardinality` to decide
visibility, so an unknown identifier and an invisible one do different work. With the visibility set
resolved once per session, the per-identifier test is a set-membership lookup, identical in work for
a gate-failed artifact and one that never existed. **No new leak-register row is needed**, and the
draft's dismissal of this route as *"hopeless for 10⁷ artifacts"* was wrong by roughly the factor
between 883 ms and never having measured it.

## M5 — not run, and why

The design carried a question about scan constants at `u8`/`u16` against the *measured* `u32`,
inherited from `filter-index.md`'s own promotion gate. **M2 and M4 make it moot here**: the
assignment column is not the chosen representation, so no artifact structure depends on a narrow
column's scan constant. The question stays open for `filter-index.md`, which has its own reasons to
want it, and is not this campaign's to close.

## M7 — the same reordering, applied to the core structures

**There is only one entity ordering.** The tiebreak is not an artifact-specific choice: taking it for
artifact membership takes it for everything in entity space at once. So the question is what else
moves, and the answer separates cleanly by what a set correlates with.

| Entity-space structure | Correlates with | Effect |
|---|---|---|
| Term postings, and any mask built from them | the **signature** — they *are* it | **none, measured 1.00×** |
| Artifact membership | **position** — a cluster is a spatial region | **4.08×, measured** |
| `permutation.bin` (entity → row) | position, by definition | **monotone within each signature group — see below** |
| Row-space anything: the projected mask, counts, tiles | nothing in entity space | **byte-identical either way** |

That last row is worth stating because it is the one a reader worries about: the set of rows a
principal can see is the set of *points* they can see, which does not depend on how entities are
numbered. Nothing row-space moves.

### The permutation becomes piecewise monotone, and this is arithmetic rather than measurement

Within a signature group, ordering entities by Morton code makes `entity → row` **monotone
increasing by construction** — not as a tendency, as a definition. The run length is therefore the
signature group size, whatever the spatial correlation happens to be. Measured on the real corpus:
**54 794 distinct signatures over 2 422 486 entities**, so

```
mean monotone run:  ~1  (today, uncorrelated)   →   ~44  under (signature, morton)
```

**The corpus already names this as the open question, and names a more expensive answer.**
`architecture.md` §5.1 says the permutation is a flat uncompressed `u32` array *"precisely because
entity order and row order are unrelated, making the values maximum-entropy; a signature-major row
layout would make it near-monotone within groups and worth compressing"* — pointing at
`deferred-signature-major-layout.md`, which is **explicitly not approved** and which changes *row*
space. The tiebreak reaches the same near-monotonicity **from the entity side, leaving row space
untouched**, so it obtains the precondition that sketch was wanted for without the sketch.

⊘ **What compression that is actually worth is not measured**, nor is its effect on
`Permutation::project`, whose gathered rows would arrive in ~44-long sorted runs rather than
scattered. Both were in this campaign's plan and neither ran — see below.

### Spatially-correlated attribute postings: expected, unmeasured

A category in a UMAP projection correlates with position, so its derived postings should compact like
artifact membership does rather than stay flat like term postings. **Not measured**, and the reason
is the next section.

## M8 — the sizing rule is shape-sensitive, and geographic regions are the adverse case

M1, M2 and M4 all measured **HDBSCAN clusters**, which are compact blobs. A region's Morton run count
tracks its **perimeter**, not its area, so the ~80 B/artifact rule is a property of compactness rather
than of membership. Same member count (40 000) throughout, only the shape varying:

| shape | runs | B/member | vs compact |
|---|---:|---:|---:|
| disc | 348 | 0.036 | 1.0× |
| square | 375 | 0.039 | 1.1× |
| coastline (fractal boundary) | 2 905 | 0.292 | **8.1×** |
| corridor (river, road, coastal strip) | 40 000 | 2.003 | **55.8×** |
| archipelago (60 parts) | 40 000 | 2.008 | **55.9×** |

**Elongated and disconnected regions degenerate completely**: 40 000 runs for 40 000 members means
the Morton curve enters and leaves once per member, and 2.0 B/member is exactly Roaring's array
container — no compression at all.

**Two things bound the concern, and both matter.** Row space is never *worse* than entity space; a
degenerate shape merely stops being better, and 2 B/member is what a scattered entity-space bitmap
costs anyway. And **the populations divide the right way**: the 10⁷-artifact families are clusterings,
which are blobs, while the shapes that degenerate — boundaries, corridors, archipelagos — number
10⁴–10⁵, where 2 B/member is affordable outright.

**The rule to carry is therefore conditional rather than flat:** ~80 B/artifact *for compact
membership*, rising to ~2 B/member for elongated or disconnected membership, with real coastlines
about 8× the compact case. A deployment whose artifacts are transport corridors or island groups
should size from the second figure.

---

## The fixtures were deleted mid-campaign

`/tmp/tessera-bench/fixtures` was cleared after M1–M4 and M6 completed and before M7 ran. Rebuilding
needs `data/geometry.parquet` and `data/corpus.parquet`, which are not on this machine
(`scripts/bench_build_fixtures.sh` filters a prefix of the source; it does not synthesise). So:

- **M1, M2, M3, M4, M6 are complete** and their numbers stand — they ran against the real bundle.
- **M7's permutation result stands**, because it is arithmetic over the signature count M3 measured.
- **M7's compression and projection arms did not run**, and cannot be re-run here.

The derived inputs that survived are in this directory (`tier_a_assign.npz` and the three JSON
result files); what was lost is `row-entity.u32`, without which nothing can be mapped back into
entity space. **Restoring the source parquets makes every arm re-runnable**, and `m7_core_rows.py`
is committed ready to run against a rebuilt fixture.

---

## What this campaign does not establish

- **Tier B's clusters are synthetic.** The generator reproduces Tier A's noise fraction, size skew
  and Morton contiguity; it does not reproduce whatever HDBSCAN does at the boundaries of a real
  cluster. Every ratio above is a function of contiguity, which is why the two tiers agree on the
  per-artifact figure — but a real 10⁹ clustering has never been measured, and cannot be here.
- **The bitmap build cost is measured, the projection cost is not.** Building 10⁷ row-space bitmaps
  from an in-memory assignment took 25.2 s single-threaded. Projecting entity-space membership
  through the permutation instead is the `Permutation::project` path, *measured* at 1 277 ms per
  25% grant (`probes/2026-08-14-project-decomposition/`), but projecting a *whole level* rather than
  a mask has not been run.
- **Nothing here is a wire or a fold measurement.** No serving path, no compaction interaction, no
  concurrency. These are structure and arithmetic only.
- **HDBSCAN on a sample, not the full corpus** (see Tier A above).
