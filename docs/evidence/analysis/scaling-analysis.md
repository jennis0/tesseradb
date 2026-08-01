# Tessera — Residency, Sharding and Scale

**Companion to** `architecture.md`. **Status: analysis, not specification.** Everything here is conditional on measurements nobody has taken. Two findings were firm enough to fold back into the design (r14, and the Phase 0 addition in the implementation plan); the rest is recorded so the reasoning is re-derivable and arguable rather than asserted.

Models sit alongside this document — `residency.py`, `scale.py`, `sizing.py`, `termshard.py`. Re-run them before trusting any number below.

---

## 0. Assumptions, stated first

Every figure in this document rests on these. Change them and the conclusions move.

| Parameter | Value | Confidence |
|---|---|---|
| Tiles per viewport | 300 | Reasonable; depends on screen and zoom |
| Rows per leaf tile | 10<sup>4</sup> | From the design |
| *k*, marks per tile | 30 | From the design |
| Page size | 4 KB | Firm |
| NVMe | QD32, 20 µs effective, 3 GB/s | Plausible; unmeasured |
| Hot columns | x, y as f32; priority quantised to u16 | Design choice |
| Visible items within a tile | uniformly scattered | **Weakest assumption**, and conservative — clustering makes both scanning and gathering cheaper |
| Semi-join throughput | 100M pairs/sec/core | Plausible; unmeasured |

The uniform-scatter assumption is the one to attack first. It is the pessimistic case, so the results are a floor rather than a forecast.

---

## 1. The query algorithm is scale-invariant

The strongest result here, and the reassuring one. Per-tile costs depend on tile capacity and coverage, **not on corpus size**:

- direct-evaluation priority block: 9.8 pages at any *N*
- gather at *k*=30: 18.8 pages at any *N*
- direct-versus-candidate-list crossover: ~5% coverage at any *N*

Nothing in §2.6 changes between 10<sup>7</sup> and 10<sup>12</sup>. What changes is **placement** — where the bytes live and how many machines hold them — and **mask management**. That separation is worth holding onto, because it means scaling work never revisits correctness work.

---

## 2. Residency: three tiers, not a binary

The framing "keep it on disk rather than in memory" turns out to be the wrong shape. Three structures behave differently enough that they want different answers.

**Hot columns and priority: resident.** At 10<sup>9</sup> that is about 10 GB — x and y at 8 GB, a u16 priority at 2 GB — which is unremarkable on any serving box. Paging them costs roughly 5,600 page reads per viewport, about 8 ms cold, which exceeds the latency budget by itself.

One counterintuitive detail: **gather cost plateaus with *k***. It is 3,870 pages at *k*=10 and 5,859 at *k*=120, because you touch a tile's pages whether you take ten points from them or a hundred. If you do end up paging, take more points per tile — the marginal cost is close to zero. This interacts usefully with the thin-client profile, where *k* is being pushed down for rendering reasons and the I/O saving from doing so is negligible.

**Masks: on disk, via frozen mmap.** They are the only structure that scales with concurrency rather than with corpus, they are mmap-native in CRoaring's frozen format, and the OS evicts idle sessions with no eviction policy to write. This is where disk residency genuinely wins even at 10<sup>9</sup>.

**Everything else: disk.** The term index (~15 GB at 10<sup>9</sup>) is read once per session and can be made local by sorting term IDs before fetching. The permutation (~4 GB per slice) is read in ascending entity order, so it is a sequential scan with gaps rather than random access. Base attributes and text are drill-down only.

So the design already sits roughly here — with pre-faulting applied to the wrong things.

**A representative column group was considered and rejected.** Storing each node's candidate list contiguously would make level-of-detail reads sequential, and at width 4*k* it is only 0.67 GB at 10<sup>9</sup> — far smaller than the earlier guess of "comparable to the hot columns", which was wrong by about 30×. But §3 shows that width serves almost nobody, and a width that does serve realistic coverage (*c*=100) lands at ~17 GB, comparable to the columns it was meant to avoid touching. It is dominated by simply keeping x, y and priority resident.

---

## 3. The two defects this analysis found

Both are now corrected in the design (r14). Recorded here because the reasoning matters more than the fix.

**Candidate lists were mis-sized.** A list of width *c·k* yields about *c·k·coverage* survivors after masking, so it produces *k* of them only above coverage 1/*c*. At *c*=4 that is 25% — and with 10<sup>4</sup> grants against 10<sup>5</sup>–10<sup>6</sup> categories, very few principals are anywhere near it. The mechanism described as "the fast path" was the path almost nobody took.

**Descent cost is linear in 1/coverage, not logarithmic.** The design said "a gradual cost increase proportional to log(1/coverage)". Depth is logarithmic, but each level multiplies the candidate pool by four, so nodes visited is the geometric sum ≈ (4/3)·*k*/(*w*·coverage):

| coverage | depth | nodes visited |
|---|---|---|
| 25% | 0 | 1 |
| 5% | 2 | 21 |
| 1% | 3 | 85 |
| 0.1% | 4 | 341 |
| 0.01% | 6 | 5,461 |

At 0.01% that is 1.6M reads per viewport — over two seconds. The understatement mattered because the descent path exists precisely to serve the sparsest principals.

**The fix was already available for free.** Direct evaluation — take the visible row IDs in the tile range from the mask, read their priorities, keep the *k* lowest — is bounded by the tile's priority block and gets *cheaper* as coverage falls. The two move in opposite directions and cross at about 5%:

| coverage | descent nodes | direct pages | winner |
|---|---|---|---|
| 25% | 1 | 9.8 | candidate list |
| 10% | 5 | 9.8 | candidate list |
| 5% | 21 | 9.8 | direct |
| 1% | 85 | 9.8 | direct |
| 0.01% | 5,461 | 1.0 | direct, by ~5,000× |

And §2.6 step 6 already computes the exact masked count per tile before selection, so the choice costs nothing to make.

This matters more than it first appears because category sizes are exponentially skewed with the largest at 25–50% of the corpus, so **coverage is bimodal**: a principal holding a head category sees half the corpus and the list works beautifully; one holding only tail categories sees a thousandth of a percent. A per-tile crossover serves both without configuration.

---

## 4. Scaling to 10<sup>10</sup>–10<sup>12</sup>

**Sharding is forced and the shard size picks itself.** Global u32 row IDs die at 4.29×10<sup>9</sup>, and monolithic structures reach 33 TB at 10<sup>12</sup>. A shard of ~10<sup>9</sup> rows keeps **per-shard row IDs in u32**, which keeps Roaring32's bounded container directory, while entity IDs go u64 globally. This is **I4 paying off in a way it was not designed for**: the entity/row split exists for a correctness reason, and it turns out to be the mechanism that keeps row space small at any corpus size.

**The real ceiling is mask cardinality, not corpus size.** Roaring's size tracks the universe above 6.25% density and the cardinality below it:

| corpus | 10% coverage | 1% | 0.01% |
|---|---|---|---|
| 10<sup>9</sup> | 125 MB | 23 MB | 230 KB |
| 10<sup>10</sup> | 1.25 GB | 230 MB | 2.3 MB |
| 10<sup>11</sup> | 12.5 GB | 2.3 GB | 23 MB |
| 10<sup>12</sup> | **125 GB** | 23 GB | 230 MB |

So the architecture carries a precondition that should be made explicit: **per-session authorised cardinality must grow sub-linearly with the corpus.** If a principal's visible set stays a constant fraction, this ceilings out around 10<sup>10</sup>. If it stays roughly constant in absolute terms — which is what compartmentalisation does as a corpus grows, since nobody is cleared for more merely because more exists — it runs to 10<sup>12</sup>. That is a claim about the access model, not the code, and it should be confirmed before any 10<sup>12</sup> promise is made.

**Masks never leave their shard.** Each shard owns a Morton range, builds its own fragment locally, and keeps it. The 125 GB figure is a whole-corpus mask nobody materialises; per shard it is 125 MB. Build cost is ~3 s per 10<sup>9</sup>-row shard and parallel, so wall-clock is flat regardless of shard count — against 312 s at 10<sup>11</sup> and 3,125 s at 10<sup>12</sup> for a full-corpus build.

**Fan-out only bites at the coarsest zoom levels.** At 10<sup>12</sup> with 1,000 shards: zoom 0 touches all 1,000, zoom 2 touches 62, zoom 4 touches 4, zoom 6 and below touches one. Panning at working zoom is a single-shard query. The expensive case is the global view, which is one answer per token that does not change as you pan — so the natural fit is for `authorise` to fan out, each shard to build its fragment and return a compact summary (masked cardinality, per-coarse-tile counts, top-*k* by priority), with full fragments staying resident on their shard. Priority sampling composes across shards for the same reason it composes across partitions.

**Disk residency inverts with scale.** At 10<sup>9</sup> the hot path is 10 GB and residency is free, so disk buys nothing. At 10<sup>12</sup> it is a fleet-cost argument — roughly 79 nodes sized for RAM residency against about 5 sized for storage capacity — an order of magnitude on the thing that dominates the bill. The ~8 ms paging cost is paid once per viewport rather than multiplied by fan-out, because at working zoom a viewport lands in one shard. The access pattern is close to ideal for a page cache: spatially localised, with region popularity almost certainly Zipfian.

**The new complexity is cross-shard version pinning.** I11 currently pins one *(segment-set version, watermark)*; across a thousand shards that becomes a pin vector, and a viewport spanning shards needs a consistent snapshot. Solvable, but it is distributed-systems work the single-node design does not have, and it is where the subtle bugs will be.

**And at some point it stops being the same product.** At 10<sup>12</sup> a fully zoomed-out map is 10<sup>6</sup> points per pixel. The interaction model inverts to filter-first, map-second, with the map as a local view of a result set rather than an atlas of everything. That is a fact about screens, not a limitation of the architecture, but it changes what gets built around it.

---

## 5. Permission-aligned partitioning — potentially the largest lever here

### 5.1 Duplication across term indexes: rejected

Storing an item once per term, contiguously, with deduplication at retrieval, is appealing: a principal holding term *T* would read a contiguous block. It fails on **counting**, not storage. An item with ten terms sits in several blocks a principal can reach, and `range_cardinality` over those blocks double-counts it — inflating counts 1.9× to 8.2× depending on how correlated a principal's grants are with an item's co-terms. Counts are the load-bearing primitive behind every density, cluster frontier, hull and label decision (**I2**), so this is a correctness failure rather than a tuning one. Deduplicating before counting means materialising the union, which is exactly the work range arithmetic exists to skip.

### 5.2 Disjoint term partitioning: sound, but keyed wrong

One item, one partition preserves additivity and sampling composition — that is §12 generalised, and it works. But partitioning *by term* fans out badly: a viewport is spatially local while a principal is term-*diffuse*, so 10<sup>4</sup> grants means reaching most term partitions on every query. Morton sharding wins for exactly the reason term sharding loses.

### 5.3 Permission-signature partitioning: the version that wins

The instinct is right; the key is wrong. What you want contiguous is not a term's items but **a principal's visible set**. Group items by the hash of their DNF term set — a **permission signature** — and a group becomes wholly visible or wholly invisible to any principal. The mask stops being a bitmap over items and becomes a union of complete ranges:

| distinct signatures | mask as runs | mask as bitmap | collapse |
|---|---|---|---|
| 10<sup>2</sup> | 400 B | 125 GB | 3×10<sup>8</sup> |
| 10<sup>4</sup> | 40 KB | 125 GB | 3×10<sup>6</sup> |
| 10<sup>6</sup> | 4 MB | 125 GB | 3×10<sup>4</sup> |

(at 10<sup>12</sup> items, 10% coverage). It also turns per-partition counts into group cardinalities — no bitmap arithmetic at all in the wholly-visible case.

**The design already contains this idea, filed as something else.** ACL-aligned clustering appears in §7.8 as the escalation if label creep makes cluster labels unservable. This analysis says it is not primarily a label-creep mitigation — it is the scaling mechanism.

**The constraint is fan-out.** Partition-major, Morton-minor: each partition internally Morton-ordered, so tiles stay contiguous within a partition and a viewport queries each reachable partition and merges. That holds to roughly 10<sup>3</sup> partitions with ~120 reachable — about 36K tile lookups per viewport. At 10<sup>4</sup> partitions with 10<sup>3</sup> reachable it collapses. Exact signatures are therefore too fine, and you would coarsen by clustering similar signatures, which reintroduces intra-partition masking but over a much denser, run-friendly mask. Graceful degradation rather than a cliff.

**Failure mode:** if signatures are near-unique per item, none of this is available and the design proceeds as specified with Morton-only sharding and scattered masks. That is the case already assumed, so this is upside rather than risk.

### 5.4 It is measurable now, for nearly nothing

The Phase 0 DNF pass already computes every item's term set. Hash it, count distinct, plot the group-size distribution. One histogram decides three things: whether permission-aligned partitioning is available, whether masks at 10<sup>12</sup> are megabytes or gigabytes, and — same root cause — most of the Morton spatial-autocorrelation question that has surfaced in every piece of analysis here. This is now in the implementation plan's Phase 0.

---

## 6. What would change the conclusions

- **Visible items cluster within tiles.** Makes direct evaluation and gathering cheaper; moves the §3 crossover; strengthens everything.
- **Signatures are near-unique.** Kills §5.3 entirely; §4 stands unchanged.
- **NVMe figures are optimistic.** Moves the §2 residency line toward keeping more resident.
- **Authorised cardinality grows linearly with the corpus.** Caps the design at ~10<sup>10</sup> regardless of everything else in §4. This is the assumption with the most leverage and the least evidence.
