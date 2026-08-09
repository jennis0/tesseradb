# Filter column layout — what it costs to know which entities a column covers

**Date:** 2026-08-08 · **Harness:** [`layoutprobe/`](layoutprobe/) · **Machine:** WSL2, 12 cores,
47 GB RAM, single-threaded

The value array is identical across every layout under consideration, so the whole question is the
**addressing structure** beside it: how a scan learns which entities a filter column covers, and
where each covered entity's value sits.

## Results

**Two constants govern the masked scan, and they are stable across three orders of magnitude.**
*These are arm 1's, over a reimplemented per-value loop, and they are what chose the layout. **The
constants to design against are arms 4, 6 and 7's**, over the shipped code: for a fixed-width column
~0.28 ns contiguous and ~9.6 ns scattered; for text 3.5–9.7 ns; and for an unselective predicate, the
result's size rather than any of these.*

| | ns per candidate entity | Measured at |
|---|---|---|
| Contiguous candidate, direct-indexed values | **~2.9** | 2.87 (1e9, 1% contiguous), 2.92 (1e9, 25%), 2.56 (1e8) |
| Scattered candidate, direct-indexed values | **~22** | 22.1 (1e9), 24.1 (1e8) |

The 7.6× penalty is random access into the value array — cache misses, not bandwidth. **This
refutes the modelling that preceded it:** the 2026-08-08 structures memo assumed 5–10 GB/s and
derived 40–80 ms for a 25% principal at 10⁹. Measured is **730 ms**, an effective ~1.4 GB/s. The
scan is bound by per-candidate work, not by memory bandwidth, and any sizing that treats it as a
bandwidth problem will be ~5–7× optimistic.

**Addressing bytes per present entity** (1e9; totals in parentheses where sub-byte):

| Presence shape | bare | runs | roaring | pairs |
|---|---|---|---|---|
| Every entity present | **0** | 0.0000 (12 B) | 0.0002 (216 KB) | 4.00 (4.0 GB) |
| Slice-blocked, 10 slices | n/a | 0.0001 (12 KB) | 0.0004 (36 KB) | 4.00 (400 MB) |
| Scattered, 10% | n/a | 10.80 (1.08 GB) | **1.25** (125 MB) | 4.00 (400 MB) |

**Scan latency, ms, at 1e9:**

| Presence / candidate | bare | runs | roaring | pairs |
|---|---|---|---|---|
| all / 1% contiguous | **28.7** | 35.0 | 1078 | 279 |
| all / 25% broad | **730** | 939 | 1518 | 870 |
| all / 1% scattered | **221** | 259 | 3684 | 1413 |
| slices / 1% contiguous | n/a | 113.8 | **109.0** | 56.6 |
| slices / 25% broad | n/a | 2873 | **161** | 800 |
| scattered / 25% broad | n/a | 10204 | **232** | 881 |

Every layout is **linear in *n*** from 1e6 to 1e9 (9.5–11.2× per decade). No cliffs, and the
ranking never changes — the choice can be made once and does not need re-deriving at scale.

## What to build

- **Presence universal → a bare value array**, entity id as the index. Free, and fastest in every
  candidate shape measured. Nothing else is competitive when it is legal.
- **Presence partial → a Roaring presence bitmap** with values stored compactly. A few tens of KB
  for slice-blocked presence, 1.25 B/present when genuinely scattered, and the only layout that
  does not degrade on a broad candidate.
- **Explicit `(entity_id, value)` pairs → never optimal**, on either axis, at any scale or presence
  shape measured. Its case is simplicity, not cost. It costs 4 B/entity **per column**: at 1e9 with
  16 filter columns, 64 GB of pure redundancy against Appendix A's ~20 GB term-index budget.

**Run tables are a trap, and this is the finding most likely to be re-derived wrongly.** A
`(start, len, base_rank)` table is unbeatable on storage — 12 bytes for an entire 1e9 column — and
that number alone recommends it. It collapses on broad candidates, because rank costs a binary
search *per candidate entity*: 2.9 s slice-blocked and 10.2 s scattered at 1e9, against Roaring's
161 ms and 232 ms. **Do not choose an addressing structure on its storage column.**

## Where the masked scan stops being affordable

**The budget is a filter budget, not a viewport budget** — owner ruling, 2026-08-08:
**0.5–1 s is acceptable for a filter change at 10⁹**, because a filter changes far less often than
the viewport does. An earlier reading of these numbers judged them against the selection path's
158–191 ms p99 and concluded an accelerator was required wherever coverage could be broad. That
was the wrong yardstick.

| Candidate shape | Affordable coverage at 10⁹, 500 ms | at 1 s |
|---|---|---|
| Contiguous | ~1.7 × 10⁸ (17%) | ~3.4 × 10⁸ (**34%**) |
| Scattered | ~2.3 × 10⁷ (2.3%) | ~4.5 × 10⁷ (4.5%) |

Measured, a 25%-coverage contiguous principal costs **730 ms** — inside the band with no accelerator
at all. Only near-total coverage exceeds it: a full-corpus scan measures ~3.0 s. **The corner is the
privileged tail, not broad coverage**, and it is the only place an accelerator earns anything.

> **Superseded by arm 4, which measures the shipped code and then makes it 11–462× faster.** These
> thresholds derive from arm 1's reimplementation at ~2.9 ns per candidate entity. The shipped scan
> is run-based and typed, and measures **~0.28 ns** contiguous, so a 1 s budget buys the whole
> corpus at 10⁹ three times over rather than a third of it, and the "privileged tail" corner closes
> outright: a full-corpus scan of a category is ~280 ms. The table is kept because the *method* —
> thresholds in candidate entities rather than bytes — is what the design uses, and because a later
> change that lost the run path would land back here.
>
> **The axis it holds constant is the one that now binds** (arm 7): these are all *selective*
> predicates. An unselective one is priced by its result, not by the scan.

## Arm 2 — does the category posting close the gap the scan leaves?

`accel` ([`layoutprobe/src/bin/accel.rs`](layoutprobe/src/bin/accel.rs), raw
[`run-accel.csv`](run-accel.csv)) answers `value == v ∧ candidate` three ways over the same
relation: the flat-column **scan**; **intersect**, which is what the built keyed reader does; and
the candidate-driven **probe** proposed to equalise work. The column is `u8`, the narrowest a
category can be and so the most favourable case for the scan.

**Yes, decisively.** At 10⁹, 25% coverage, worst posting shape:

| Value's share of the corpus | scan | intersect | probe |
|---|---|---|---|
| 0.1% | 785 ms | 0.13 ms | 5.0 ms |
| 1% | 795 ms | 0.78 ms | 10.6 ms |
| 25% | 1,351 ms | 3.56 ms | 38.3 ms |

The worst cell measured anywhere — scattered posting, 25% of the corpus carrying the value, **full**
coverage — is 5,271 ms scanned against **49.5 ms** intersected, a 107× reduction that lands inside
the 158–191 ms operating point. Postings are also nearly free to store: a correlated posting is
8 B–54 KB at 10⁹, a scattered one 2–125 MB, against a 1 GB `u8` column.

**The corpus's 21.7 ms / 2,885 ms warning does not apply to this operation and should not be cited
against it.** That spread is a union over ~10⁴ *term* postings during `M_auth` construction. A
single value intersected against a candidate is one bitmap operation and never enters that regime.

### The hidden-value channel, measured directly

per-point-attributes §3.8 requires that a value the principal cannot see be indistinguishable **in
work** from a value that does not exist. Both return nothing; only timing could separate them. The
`hidden-25pct` rows place a 25% candidate **disjoint** from a correlated value's members, so the
principal sees none of them, and time it against a value with no members at all.

At 10⁹, 25% coverage:

| Value | intersect | probe |
|---|---|---|
| Does not exist (no members) | 0.000 ms | 1.51 ms |
| Hidden, 1M members | 0.000 ms | 1.05 ms |
| Hidden, 10M members | 0.001 ms | 1.18 ms |
| **Hidden, 250M members** | **0.000 ms** | **1.06 ms** |

**No measurable channel, at either evaluation form.** Roaring's intersection short-circuits on
container keys, so a posting whose containers do not meet the candidate's costs a key-list merge and
nothing more, however many members it holds. The plain intersect is as flat as the candidate-driven
probe here, and the probe's ~1.1 ms floor buys nothing the intersect does not already give.

**Read this narrowly.** Work still scales with the *visible* result — 38 ms for a value 62M of whose
members the principal can see — but that is not a disclosure, since the principal is entitled to
those results. The measured absence of a channel is for the **correlated** shape only. A uniformly
scattered value cannot be fully hidden from a broad principal in the first place (its members are in
every container), but the intermediate case — a scattered posting whose containers the candidate
meets while no bits match — is **not measured**, and reasoning says work there is container-
proportional while the result is empty. That case, not the one measured, is where a residual could
live.

## Arm 4 — the shipped scan, and the two optimisations that took it 13–272× faster

Arms 1–3 reimplement the scan in this crate, which is right for comparing *layouts* but means their
constants describe the approach rather than the code. `realscan`
([`layoutprobe/src/bin/realscan.rs`](layoutprobe/src/bin/realscan.rs), raw
[`run-realscan.csv`](run-realscan.csv) — three runs per scale, medians below) calls
`tessera_filter::ValueColumn::scan_eq` directly, over both presence shapes.

**The reimplementation was optimistic, and by enough to matter.** At 10⁹ and 25% coverage the model
says 730 ms; the shipped code said **900 ms** — 23% slower, because the model's loop and the shipped
`walk` were not the same loop. Any optimisation judged against arm 1's numbers would have been judged
against itself.

### What changed

Two independent changes, measured separately because the second one regressed a cell the first had
improved.

**(1) Iterate runs, not values** — the same observation applied to both presence shapes.

- **Universal presence** — the entity id *is* the array index, so a candidate run is a contiguous
  slice of the value column. The loop becomes a bulk range read and a plain integer walk.
- **Partial presence** — slot *k* is the *k*-th set bit of the presence bitmap, so a naive walk
  steps it one bit at a time and costs O(present) however small the candidate is. But rank is
  **affine inside a run**: within a presence run starting at `ps` with `base` bits before it, entity
  `e` is at slot `base + (e − ps)`. Merging the two bitmaps' runs gives every slot by arithmetic, at
  O(runs).

**(2) Traverse the column at its own type.** The scan dispatched on the `Codes` variant *per
element* and compared through a widened `Scalar`, so every value paid a match and a widening. The
traversal is now monomorphic per column type, and a numeric bound is narrowed to the column's native
type **once per scan** — which also settles the degenerate cases once rather than a billion times: a
bound below the type's floor constrains nothing, one above its ceiling excludes everything, and a
fractional bound on an integer column rounds outward.

### Measured, at 10⁹ — medians of three

| Presence | Candidate | Original | + runs | + typed | Total |
|---|---|---|---|---|---|
| universal | 1% contiguous | 30.97 ms (3.10 ns) | 9.81 ms | **2.82 ms (0.28 ns)** | **11×** |
| universal | 25% broad | 900.51 ms (3.60 ns) | 201.55 ms | **74.46 ms (0.30 ns)** | **12×** |
| universal | 1% scattered | 276.75 ms (27.67 ns) | 161.90 ms | **96.45 ms (9.64 ns)** | 2.9× |
| slice-blocked | 1% contiguous | 124.89 ms (12.49 ns) | 0.89 ms | **0.27 ms (0.03 ns)** | **462×** |
| slice-blocked | 25% broad | 176.85 ms (0.71 ns) | 18.97 ms | **6.65 ms (0.03 ns)** | 27× |
| slice-blocked | 1% scattered | 402.97 ms (40.29 ns) | 27.25 ms | **18.46 ms (1.85 ns)** | 22× |

(The "+ typed" column carries the **final** figures, re-measured after arms 6 and 7 against the code
as it now stands: the universal arms are unchanged within noise throughout and the partial-presence
arms improved with the shared traversal.)

Results are identical throughout; only the timings move.

**The 272× is the O(present) term disappearing**, and it is the largest single win in this campaign.
The old walk paid for the *whole* presence bitmap whatever the candidate asked for, so a 1% candidate
over a 10%-present column did a hundred times the necessary work. Arm 1 measured that shape as its
worst cell and attributed it to the addressing structure; it was the rank algorithm.

**The typed traversal regressed the scattered arm before it improved it**, and that is the finding
most worth carrying forward. Its first form took scattered from 161.90 ms to **194.34 ms** — a 20%
regression — because **a scattered candidate is one-element runs**, and constructing a slice iterator
per run costs more than the direct index it replaced. A length-1 fast path fixed it and then
overtook the previous figure. A change measured only on the contiguous arm would have shipped that
regression, on precisely the shape a poorly-correlated attribute produces.

**The constants to design against are ~0.24 ns per candidate entity contiguous and ~10 ns
scattered** for a universal column, and ~0.05 ns / ~1.8 ns for a partial one — the presence bitmap
having become a *filter* on work rather than a tax on it. A whole-corpus scan at 10⁹ is **~240 ms**,
against ~3.0 s when this campaign started.

**The scattered arm is the noisy one** — 98–110 ms across runs against ±3% for the others, because
it is the only cell bound by random access into the value array. Quote it as a range.

**Neither change touches the timing property.** Run structure belongs to the candidate, presence to
the column, the traversal to the column's declared type; none is a function of the value sought. The
optimisation that would not be safe — stopping once the result is complete — remains forbidden. Both
also degrade to the old cost rather than past it: a scattered candidate is one run per entity, which
is what the per-value loop was already paying.

**Not taken**, and a separate decision: the scan is single-threaded. Splitting by container is
embarrassingly parallel, but it borrows capacity from concurrent requests, which the
compute-admission gate exists to ration — so it is a concurrency decision rather than a free win.

**The residency problem this does not touch** is arm 5's.

## Arm 5 — what a generation pays to *open* its filter columns

Every declared filter column is opened at once at generation open and held for the process lifetime,
so residency is a term a deployment pays whether or not anyone ever filters. `residency`
([`layoutprobe/src/bin/residency.rs`](layoutprobe/src/bin/residency.rs), raw
[`run-residency.csv`](run-residency.csv)) opens eight 10⁸-entity `u32` columns — 3.2 GB of values —
and reads RSS from `/proc/self/statm`, which counts *resident* pages rather than virtual size. One
arm per process, because the read arm's freed pages would otherwise sit in the allocator's arena and
flatter the mapped one.

| | Open | Resident after open | After one 1% scan | Scan, cold | warm |
|---|---|---|---|---|---|
| read into memory | 2,196 ms | **3,301 MB** | 3,301 MB | 0.35 ms | 0.27 ms |
| mapped | **0.2 ms** | **2 MB** | 6 MB | 0.26 ms | 0.24 ms |

**Three orders of magnitude on open latency and 1,650× on resident bytes, and the scan is
unaffected.** The mapped arm resides only what it touches: 2 MB at open, 6 MB after a scan that
reads 4 MB of one column. Extrapolated at the ratio the design sizes against — 10⁹ entities, 16
declared columns at `u32` — the read path is **64 GB resident before a single filter arrives**, and
the mapped path is the working set of whatever is actually scanned.

**The scan figures here are warm-page-cache and should not be read as a cold-start claim.** The
files had just been written, so neither arm paid disk. What the comparison does establish is that
mapping costs the scan nothing once the pages are resident, and that the read arm pays its I/O for
**every declared column** at open where the mapped arm pays it only for the columns a request
touches. A genuinely cold first scan would pay disk on either path; only the read path pays it
sixteen times over for columns nobody asked about.

**This is why `FilterColumns::open` takes an `mmap` flag and the engine passes `true`** — the same
construction and the same argument as `PostingsReader::open`, which the auth index has used since it
was built.

## Arm 6 — the two families the other arms never measured

Arms 1–4 all measure `scan_eq` over a fixed-width column, which is a category's *equality* case and
nothing else. `textscan`
([`layoutprobe/src/bin/textscan.rs`](layoutprobe/src/bin/textscan.rs), raw
[`run-textscan.csv`](run-textscan.csv), 10⁸, medians of three) covers what a filter surface actually
issues: the four **text** predicates, and **category set membership** at each declared width. Values
are surname-shaped — a small stem vocabulary with a numeric tail — so they share long prefixes,
which is the adversarial case for a byte comparison and also what a real string column looks like.

**Both families were substantially slower than equality, and both for fixable reasons.**

| Predicate | Before | After | |
|---|---|---|---|
| `text eq` | 11.05 ns | **3.54 ns** | 3.1× |
| `text prefix` | 14.31 ns | **4.50 ns** | 3.2× |
| `text in` (5 values) | 25.23 ns | **8.32 ns** | 3.0× |
| `text contains` | 40.35 ns | **9.67 ns** | 4.2× |
| `u8 in` (32 values) | 5.52 ns | **2.51 ns** | 2.2× |
| `u16 in` (32 values) | 2.97 ns | **0.46 ns** | 6.5× |
| `u32 in` (32 values) | 3.08 ns | 3.41 ns | unchanged |

(ns per candidate entity, 25% broad candidate. `category eq` is 0.22–0.30 ns and did not move.)

**The text cost was UTF-8 validation, run per value per request.** Resolving a slot to a `&str` ran
`std::str::from_utf8` over the value — for every candidate entity, on every request, over bytes
Arrow had already validated when the column was opened. The predicates now compare bytes, which
answers the same question: UTF-8 is self-synchronising, so a valid needle cannot match starting
part-way through a character. Text also never received arm 4's typed traversal, because its values
are not a slice of anything; giving the traversal a *slot-range* interface rather than a
*single-slot* one let text walk offset pairs with the same freedom from bounds checks that the
fixed-width arm walks values with.

**`in` cost was a search per candidate entity, and the fix differs by width.** A `u8` or `u16`
category's whole domain fits in a bit table — 32 bytes or 8 KB, built once per scan — so membership
becomes a constant-time lookup and a 32-value set costs what a 2-value set costs. That is visible in
the `u16` row above: 0.45, 0.45 and 0.46 ns at k = 2, 8 and 32. A `u32` domain is 4×10⁹ codes and is
not a table, so it keeps a sorted list and stays O(log k); text keeps a first-byte bucket index,
which turns a set membership into zero or one full comparison.

**The bit table is also the better security property, not merely the faster one.** Its work does not
depend on *which* codes are asked for, so a code no entity carries and a code the principal cannot
see cost the same table build and the same lookup — which is what per-point-attributes §3.8 requires,
arrived at by construction rather than by care.

**Text remains ~14× a category's equality cost, and that is now close to a floor.** Per value the
scan streams two 8-byte offsets and the value's bytes — about 22 bytes against a `u32` column's 4 —
so 3.4 ns is roughly memory bandwidth for the shape. Halving the offset width when a column's bytes
fit in 4 GiB would take about a fifth of the remaining traffic; it is not taken here, and is
recorded as the next thing to try if text ever needs to be faster.

**A scattered candidate is the worst case for text by a wide margin** — 30 ns for equality against
3.5 contiguous, and 96 ns for `contains` — because each value is a separate random access into the
byte array rather than a stride through it. At 10⁹ a scattered 1% `contains` is ~1 s: at the edge of
the budget, and the one cell in this campaign where a text filter and a poorly-correlated principal
together would exceed it.

## Arm 7 — what an **unselective** filter costs, which every other arm holds constant

Arms 1–6 all use predicates matching a fraction of a percent of the candidate, because they were
measuring the traversal. A filter surface issues unselective predicates routinely — a range covering
most of a domain, a tick-box set with everything ticked — and there the *result*, not the scan, sets
the cost. `selective`
([`layoutprobe/src/bin/selective.rs`](layoutprobe/src/bin/selective.rs), raw
[`run-selective.csv`](run-selective.csv), 10⁹, medians of three) sweeps the share of the candidate
that matches, and reports peak RSS as well as time.

**The result was accumulated whole before it became a bitmap, and that was a memory problem before
it was a latency one:**

| Candidate | Matches | Before | | After | |
|---|---|---|---|---|---|
| 25% broad | 1% | 167 ms | 10 MB | 192 ms | **0 MB** |
| 25% broad | 25% | 878 ms | 250 MB | 857 ms | **0 MB** |
| 25% broad | **100%** | 1,556 ms | 1,000 MB | **218 ms** | **0 MB** |
| whole corpus | 1% | 683 ms | 40 MB | 768 ms | **1 MB** |
| whole corpus | 25% | 3,701 ms | 1,112 MB | 3,390 ms | **105 MB** |
| whole corpus | 50% | 7,020 ms | 2,111 MB | 6,037 ms | **105 MB** |
| whole corpus | **100%** | 7,306 ms | 4,112 MB | **878 ms** | **0 MB** |

**A filter matching a quarter of a 10⁹ corpus allocated 1.1 GB transiently, per concurrent request**,
and one matching all of it 4.1 GB — on a request path, for a quantity the compute-admission gate
rations CPU for and knows nothing about. Folding the buffer into the bitmap every 64 Ki entities
bounds it at 512 KB whatever the result's size; the 105 MB that remains is the Roaring bitmap the
result genuinely is.

**Consecutive matches now go in as a range**, which is what takes the fully-matching cases from 7.3 s
to 878 ms — an unselective predicate matches in long contiguous stretches by nature. Below a 32-entity
threshold the entities take the bulk path they always took, so this cannot cost the middling case,
where matches come in pairs rather than runs.

**The middling case is unchanged and it is outside the budget.** A predicate matching 25–50% of a
whole-corpus candidate costs 3.4–6.0 s, and after these changes essentially all of that is croaring
building a 250–500 million-entity bitmap — `add_many` already uses CRoaring's bulk context, so there
is no cheap win left in it. **This is the largest known gap in the filter path**, it is a property of
the result's size rather than of the scan, and no accelerator over the *column* would touch it.

### Where the unselective time actually goes — and a fix that measured well and was still refused

`ceiling` ([`layoutprobe/src/bin/ceiling.rs`](layoutprobe/src/bin/ceiling.rs), raw
[`run-ceiling.csv`](run-ceiling.csv)) times the same traversal over the same 10⁹ column three ways:
counting matches without building anything, collecting them into a flat buffer, and folding that
buffer into a bitmap. The first is the floor — no result representation can beat not building one.

| Selectivity | count only | collect | `add_many` |
|---|---|---|---|
| 1% | 340 ms | 535 ms | 58 ms |
| 25% | **1,504 ms** | 2,569 ms | 1,076 ms |
| 50% | **2,515 ms** | 4,127 ms | 1,947 ms |
| 100% | **328 ms** | 3,089 ms | 3,883 ms |

**Counting alone is 4.6× dearer at 25% than at 100%, over identical work.** That is branch
misprediction: the predicate is a coin flip at middling selectivity and perfectly predictable at
both extremes. So a large share of the unselective cost is not croaring at all and not the memory
traffic — it is the `if` in the inner loop.

The standard fix is to remove the branch: store the entity unconditionally and advance the cursor by
the predicate. In isolation it works exactly as advertised — **flat at ~445 ms across the whole
selectivity range**, against 332 ms at 1% and 4,330 ms at 50%, a 10× improvement where it is worst
and a crossover at ~3%:

| Selectivity | 1% | 5% | 10% | 30% | 50% | 90% | 100% |
|---|---|---|---|---|---|---|---|
| branchy | 332 | 660 | 1,040 | 2,729 | 4,330 | 3,246 | 3,569 |
| branchless | 448 | 440 | 432 | 441 | 449 | 448 | 490 |

**Built into the real scan it was a bad trade, and is not taken.** Measured end to end, it cost the
selective arms 1.5–2.1× (arm 4's contiguous cell 2.82 → 4.22 ms, the 1%-selectivity whole-corpus
scan 768 → 1,647 ms) and bought only 1.3–1.6× on the unselective ones (25%: 3,390 → 2,637 ms; 50%:
6,037 → 3,677 ms) — which remain far outside the budget either way. The isolated 10× does not
survive contact because `add_many` is 1.0–1.9 s of the unselective total and branchlessness does
nothing for it, while the extra 4 bytes written per *candidate* entity is pure added traffic for a
selective predicate. A viewer filtering to one category value is the common case and it is
selective.

Two things follow for whoever revisits this. The measurement stands even though the change did not:
**the unselective cost decomposes into ~40% mispredicted branches, ~30% `add_many`, ~30% buffer
traffic**, and any fix has to address more than one of them to matter. And branchless would become
attractive alongside a cheaper result representation, since it is the *combination* — not either
alone — that reaches the budget.

### Three regressions on the way, all caught by re-running arms 4 and 6

None of this was visible in the arm being optimised, and each cost more than the change was worth:

- **Capping the slot ranges by wrapping the callback** in a splitting closure cost the scattered arm
  a doubling, 98 → 239 ms, because the extra closure layer stopped the predicate inlining into the
  traversal. A scattered candidate is one call per entity, so an indirection there is paid ten
  million times. The cap turned out to be unnecessary once the buffer was bounded in `push`, where
  the check is per *match* rather than per candidate entity.
- **`run_optimize` on every result** cost the partial-presence arms 0.25 → 0.45 ms and 6.2 →
  12.7 ms, walking every container of a result that had no runs to find. It now runs only when the
  coalescing actually fired, which is exactly the case that pays for it.
- **`push` carrying its cold path inline** — the branch that retires a stretch calls into croaring —
  cost the scattered arm 34%, for the same inlining reason as the first. Moving it behind
  `#[inline(never)]` restored parity.

The published arm 4 and 6 figures are re-measured against the final code and are at parity with the
pre-arm-7 ones.

### What the fix cost the other arms: nothing, and it helped one

Sharing the traversal meant re-running arm 4. The universal-presence figures are unchanged within
noise (0.25 / 0.27 / 10.98 ns), and the **partial-presence arms improved** — 0.46 → 0.30 ms
contiguous and 11.50 → 6.23 ms broad at 10⁹ — because the merged run bound is now computed once per
range rather than per slot. The recorded arm 4 table carries these figures.

## Arm 3 — getting the result into row space

`project` ([`layoutprobe/src/bin/project.rs`](layoutprobe/src/bin/project.rs), raw
[`run-project-1e9.csv`](run-project-1e9.csv)) compares the two routes from an entity-space filter
result to the row-space answer a viewport needs. **project** gathers `entity_to_row` over the
result's set bits, sorts and builds a row-space bitmap — O(set bits). **per-tile** never projects:
for each of the ~300 tiles a viewport resolves to, it walks that tile's contiguous row range, maps
row → entity and tests membership — O(rows in the viewport), independent of the result's size.
`entity_to_row` is a genuine Fisher–Yates permutation, because both routes are dominated by random
access and a structured map would flatter them equally and wrongly.

**Two constants, at 10⁹:**

| Route | Cost | Scales with |
|---|---|---|
| project | **~27 ns per set bit** | the result |
| per-tile test | **~6–22 ns per viewport row** | the viewport |

| Result cardinality | project | per-tile, 300 tiles × 1,000 rows |
|---|---|---|
| 10⁵ | **3.8 ms** | 1.73 ms |
| 10⁶ | 27.1 ms | **2.37 ms** |
| 10⁷ | 271.7 ms | **3.79 ms** |
| 10⁸ | 2,779 ms | **6.49 ms** — 428× |

**The rule: project only when the result is smaller than roughly a quarter of the viewport's row
count; test per tile otherwise.** The ratio is just the two constants. At a 300,000-row viewport the
crossover is a result of ~10⁵, and both scales agree on it (~1/4 at 10⁸, ~1/3 at 10⁹).

**The asymmetry is what matters for the design.** The per-tile route is bounded by the viewport,
which is already bounded by the drawn-mark budget, so it scales with neither the corpus nor how much
the filter matches. Projecting a 10⁸-entity result costs 2,779 ms — **more than the 730 ms scan that
produced it** — so for any broad filter the projection, not the scan, was the dominant term, and
this route removes it.

**These project figures are a floor, not a worst case.** The results here are *contiguous*, which is
the cheap end for a gather. The corpus's 127 ns/set-bit point (`probes/results.md` §6, a 69.3×10⁶
mask) is ~4.7× this arm's constant and is the likely shape of a scattered one; §10.4's 10.7 ns/item
at 10⁹ is a different operation again. **The per-bit constant is shape-dependent and no design should
quote a flat one.**

## What this does not settle

- **Value width.** Everything here is a `u32` column. A `u8` category quarters the value bytes but
  not the per-candidate constant, so the scattered-candidate case (cache-miss bound) should improve
  more than the contiguous one (work bound). Not measured.
- **Strings.** Variable-width values change the access pattern entirely and are not modelled here.
- **Parallelism.** Single-threaded throughout. A real scan would be parallel across candidate
  containers; the ranking between layouts should be unaffected, but the absolute budget above would
  move.
- **Disk.** In-memory throughout, because the machine had 4.7 GB free. A cold scan is a different
  measurement and the 2.9 ns constant does not transfer to one.
- **The accelerator crossover.** Arm 2 settles this for categories. For *numerics* it is still open:
  neither zone maps nor bit slicing is built, and which (if either) should be is an owner ruling
  (`filter-index.md` §3). If bit slicing is ever taken, its modelled ~4 GB per `u32` column at 10⁹ —
  derived from an assumed half-density, never measured — needs a gate arm before it is believed.
- **The row-space projection of a filter operand, against operand cardinality.** The corpus holds two
  measured points that do not agree per-bit: 8.8 s for a 69.3×10⁶-entity mask (`results.md` §6, i.e.
  127 ns/set-bit) and 10.7 s at 10⁹ (§10.4, i.e. 10.7 ns/item). The constant is evidently
  cardinality-dependent, and no curve exists between them. Any design quoting a flat per-bit figure is
  quoting one end of an unmeasured range.
- **The build's peak resident set, per family.** The current emit holds a `u32` per non-absent entity
  per column on top of an attribute tail already priced as outside the memory plan at 10⁹.

## Method

`layoutprobe` builds one synthetic column of 1,000 distinct `u32` values under three presence
shapes, and scans it for a single value under three candidate masks, in four layouts. Presence
shapes: universal; ten slices interleaved in 10⁵-entity blocks (modelling concurrent multi-slice
ingest, where write-path §4.2 makes an entity range ascending-*with-holes*); and 10% scattered.
Candidates: 1% contiguous (a sparse principal under signature-sorted assignment), 25% contiguous
(a head principal), 1% scattered (the `surnames` policy shape, where signature sorting measured
~1.0×).

Every layout holds the same logical relation, and the harness asserts all four return **identical
answers** before any timing is reported — without which the fast ones are only fast. Each cell is
preceded by an untimed warm pass. Roaring sizes are *serialised* (`Portable`), which is what a
bundle would store, not the in-memory footprint.

Reproduce: `cargo build --release && ./target/release/layoutprobe 1000000 100000000 1000000000`.
