# `N_occ(d)` by a sketch: one arm instead of three

Status: measurements taken 2026-09-09 on branch `probe/nocc-sketch`. **Not normative, and nothing here is settled.** [Decision 0137](../../docs/decisions/0137-theta-is-anchored-on-the-occupied-tile-count.md) is not amended and no design document has been changed. Re-take a figure before relying on it — see *What was not measured*.

## What was asked

`occupied_tiles` computes `N_occ(d)`, θ's second anchor (§7.2). It is exact, and three branches exist to make it fast: `main` (a counter at one segment, a Roaring union above), `perf/nocc-merge-accumulator` (a sort), and `perf/nocc-tiered-accumulator` (a counter, a direct-mapped bitset to depth 12, a compacting sorted buffer below). Three arms, a tuning constant and a time/memory trade, for a scalar.

`N_occ` is a distinct-count, and §7.2 needs two things from it — monotone in depth, computed inside the viewer's mask. Neither is exactness. The question was whether a HyperLogLog replaces all three arms at once, since sketches merge by taking the maximum per register, which **is** the union.

## Method

`tessera-bench`'s `occupancy_sketch`, which is `occupancy_segments`'s fixture, split shapes and instruments with a new measurement core. Each corpus's own Morton column, inverted through `tessera_spatial::unsplit32` and re-dealt into 1 to 512 segments; the visible set defined over *points*, so `N_occ` is constant along the segment axis by construction and the harness asserts it; a counting `#[global_allocator]` with `croaring::configure_rust_alloc` routing CRoaring's `malloc` through it, reporting **peak live bytes**.

**Four complete routes — walk and accumulator together — priced against one mask in one process**, so the comparison is not between binaries:

* **union** — `main`'s arm.
* **tiered** — `perf/nocc-tiered-accumulator` at `edbcba72`, transcribed with its own constants (`BITSET_MAX_DEPTH` 12, `COMPACTION_SLACK` 4, `FIRST_COMPACTION` 2¹⁶).
* **one sketch per depth** — a sketch at the requested depth and nothing else.
* **the ladder** — one walk at depth *d* filling every rung `0..=d`, with the `4^d` clamp and the running maximum applied.

An **exact ladder** rides beside them: the same one walk and the same ancestor descent, with seventeen exact accumulators behind it instead of seventeen sketches. It is the sketch's real competitor for the fill-every-shallower-rung claim, which is orthogonal to whether the count is exact.

Accuracy is against `exact_ladder`, a linear scan over every visible row that shares no code with the walk or the sketch, cross-checked at `--oracle` against a full hash-set scan.

Corpora: `treeoflife-1m` (`bioclip` view, 10⁶ rows) and `geonames` (`bundle-final`, 1.35 × 10⁷ rows), read as `morton.u32` directly — no deployment is opened.

## Figures

### What a session pays for the whole ladder

The sum over all seventeen depths, whole mask, milliseconds, minimum of three timed calls.

| corpus, split | S | union | tiered | one sketch per depth | ladder, deepest first | ladder, refilled at every depth |
|---|---|---|---|---|---|---|
| `treeoflife-1m`, flush | 1 | 24.0 | 24.0 | 35.9 | **24.8** | 104.5 |
| | 16 | 60.2 | 36.5 | 36.3 | **25.2** | 107.1 |
| | 256 | 163.9 | 62.9 | 46.6 | **29.3** | 152.7 |
| | 512 | 243.9 | 97.5 | 59.7 | **33.0** | 187.8 |
| `treeoflife-1m`, interleave | 1 | 24.8 | 24.2 | 37.1 | **24.8** | 119.1 |
| | 16 | 297.5 | 71.8 | 46.8 | **31.5** | 169.2 |
| | 512 | 412.7 | 150.3 | 78.8 | **40.7** | 277.2 |
| `geonames`, flush | 1 | 218.2 | 217.0 | 263.2 | **189.1** | 533.8 |
| | 16 | 426.7 | 376.0 | 270.1 | **185.4** | 551.7 |
| | 256 | 1015.1 | 663.1 | 367.7 | **228.9** | 896.3 |
| | 512 | 1684.9 | 1093.7 | 514.6 | **276.1** | 1285.7 |
| `geonames`, interleave | 1 | 224.2 | 214.7 | 271.7 | **195.9** | 563.5 |
| | 16 | 2538.0 | 948.6 | 418.9 | **271.6** | 1076.1 |
| | 512 | 7026.8 | 2139.9 | 1013.1 | **593.7** | 2927.9 |
| `geonames`, flush, 5% mask | 1 | 103.5 | 103.3 | 117.0 | **20.5** | 160.7 |
| | 16 | 134.9 | 116.3 | 121.5 | **21.2** | 160.0 |
| | 512 | 271.3 | 161.8 | 141.9 | **29.5** | 268.9 |

The **5%-mask row is where the ladder's whole argument is visible**. A sparse mask is many visible runs, and a per-depth walk pays that fixed cost seventeen times: the union takes 103.5 ms over all seventeen depths at one segment where one walk that fills the whole ladder takes 20.5. The other rows are whole-mask, which is one run per segment and the walk's cheapest case.

The two sketch columns that matter are **"one sketch per depth"** and **"ladder, deepest first"**. They are the two bounds on one policy: a request at depth *d* walks once and fills every rung `0..=d` not already memoised, so a session that jumps to its deepest zoom and works outwards pays one walk for the whole ladder (the right-hand bound), and one that steps down a level at a time finds each shallower rung already filled and pays one walk per level with only that level's sketch to update (the left).

**"Refilled at every depth"** is the naive policy — rebuild the whole ladder on every miss — and it is a loss at every cell measured. It is in the table because it is what "one walk fills every depth below it" costs if the memo is not consulted first, and it is three to five times the deepest rung's emissions.

The full sweep — 1, 2, 4, 8, 16, 64, 256 and 512 segments, three split shapes, both corpora, and a 5%-visible mask — is in the JSON beside this file. Against the tiered arm, over all 64 (corpus, split, S) cells:

| | worst bound (one sketch per depth) | best bound (ladder, deepest first) |
|---|---|---|
| one segment (8 cells) | **0.61× to 0.88×** — the sketch loses at every one | 0.97× to 5.04× |
| two or more (56 cells) | 0.79× to 2.54× | 0.99× to 6.15× |

At one segment the walk emits each tile once and a counter *is* the accumulator, so the sketch is paying a hash for something that was free. Above one segment the accumulator has real work to do and the sketch's has none. The ladder's best bound is at or above parity at 62 of the 64 cells; the two below it — `treeoflife-1m` contiguous at 4 segments (0.99×) and at one (0.97×) — are the split shape that makes the segments' tile sets nearly disjoint, which is the accumulator's easiest case and is kept as a bound rather than as the expected one. Under decision 0091 a live view accumulates a segment per flush, so one segment is a freshly built bundle and nothing else.

### Accuracy

1,088 cells: both corpora, all three split shapes and a 5% mask, 1 to 512 segments, every depth 0 to 16.

| | precision 14 (16 kB a rung) | precision 12 (4 kB a rung) |
|---|---|---|
| median \|error\| | 0.465% | 1.055% |
| p90 | 1.430% | — |
| p99 / max | 2.041% | 4.082% |
| inversions before the running maximum | **16** | — |
| inversions after it | **0** | — |

The worst cell is `treeoflife-1m`, one segment, depth 3: 49 occupied tiles read as 48. Every depth 0, 1 and 2 cell is exact, because the `4^d` clamp binds there.

**What that error is in the quantity that matters.** θ_d = `m_target · N_occ(d) / V_total` is linear in `N_occ`, so a 2.04% error in `N_occ` is a 2.04% error in θ and a 2.04% error in the mean marks per occupied tile: at `m_target` = 16 the mean tile draws 15.67 marks instead of 16. At the median cell it draws 15.93.

**Sixteen inversions appeared, and the running maximum absorbed every one.** They are all the same cell, at every segment count: `treeoflife-1m` under a 5% mask, depths 14 to 15, where the raw estimate falls from 50,352 to 50,187 — 0.33% — because `N_occ` barely grows between those two levels and the two estimates land either side of it. That is the case the running maximum exists for, and it is not hypothetical. Without it θ would shrink on that zoom step and the child tile would draw fewer marks than its parent, which is exactly what §7.2's nesting proof forbids.

### Memory

Peak live bytes during the call, the maximum over each whole sweep.

| corpus | union | tiered | exact ladder | sketch ladder |
|---|---|---|---|---|
| `treeoflife-1m` | 3.35 MB | 8.19 MB | 23.6 MB | **279 kB** |
| `geonames` | 22.1 MB | 93.2 MB | 264 MB | **279 kB** |

The sketch's 279 kB is `17 × 2^14` bytes and is the same number at every segment count, every depth and both corpora. Nothing in the arm grows with the data.

### The conformance differential stayed exact

`reference/oracle/occupancy.py` reproduces the sketch bit for bit. `conformance/tests/test_i7_selection.py` passes 25/25 unchanged — no tolerance was introduced.

It is live rather than vacuously passing: changing the oracle's `SKETCH_SEED` by **one** makes `test_i7_selection_differential[full_100pct-live]` fail on a served-set disagreement. Only that one of the 25 cases is sensitive, because the rest have masks small enough for the estimate to be exact or a θ that saturates. That is a real limit on how much of the sketch the differential exercises, and it is why the two implementations are also pinned directly against each other, vector for vector, in `the_ladder_matches_the_python_oracle_vector_for_vector` and `test_the_ladder_matches_the_engine_vector_for_vector`.

Bit-exactness is bought by using **no floating point at all**: the harmonic sum is an exact integer in units of `2^-(65-p)`, α is an exact rational, and the small-range branch's logarithm is a fixed-point `atanh` series. There is no libm, no summation order and no rounding mode for the two implementations to disagree over.

## What it costs

**A single tile stops being observable.** `n_occ_falls_when_a_suppression_empties_a_tile` asserts that emptying one depth-16 tile lowers `N_occ` by one — the surface on which the I2 property, that the anchor is composed rather than pre-overlay, is pinned. Under the sketch the fixture's thousand occupied tiles read 1,008, and the fall is still exactly one, so the test passes with one constant changed. That is a property of the fixture's **size**: at a thousand distinct values in 2¹⁴ registers almost every tile owns an uncollided register. At a million occupied tiles a single emptied tile is far inside the sketch's error and no such test could be written. The property survives; the resolution at which it can be asserted does not.

**The single-segment case gives up an exact answer that was free.** One segment's walk emits ascending and without repetition, so a counter *is* the accumulator and the exact ladder's seventeen counters are exact and cost nothing. The sketch replaces them with `Σ_d N_occ(d)` hash-and-write operations. Measured at one segment over all seventeen depths: exact ladder 12.5 ms against the sketch ladder's 24.8 over 10⁶ rows, and 129 ms against 189 over 1.35 × 10⁷.

**The exact ladder is a real alternative and is faster below about 64 segments.** It is the same one walk with seventeen exact accumulators, and it beats the sketch ladder at 1 to 16 segments on `treeoflife-1m` and at 1 segment on `geonames`. What it costs is memory: 23.6 MB and 263 MB peak, against 279 kB, at a depth the client chooses on every request.

## Three normative statements this contradicts, and one it does not

**Architecture §7.2 forbids the running maximum, in terms.** *(r62)*: "No implementation may clamp θ or carry a running maximum over depth: a clamp would conceal a miscount rather than prevent one." That sentence is right about an exact count — a fall in `N_occ` between two depths can only be a bug. It is the reverse under an estimate: the fall measured here is estimator noise over a quantity that grew by less than the sketch's error, and nothing about the walk is wrong. But the prohibition is normative and `architecture.md` wins any conflict, so **this is an owner ruling, not an implementation choice**, and this branch is in breach of the specification until it is made.

**The oracle's independence is not what r62 says it is.** *(r62)*: "The reference oracle counts `N_occ` by bucketing every row's recomputed tile, where the engine gallops the stored Morton column, so the differential's independence moves from the arithmetic to the count." Under a sketch the oracle no longer counts — it transcribes the engine's estimator, exactly. What stays independent is the *tile set*: `Selection` recomputes each row's tile from the source geometry where the engine gallops the stored column, so a build that wrote a wrong Morton column still fails. What is no longer independent is turning that set into a number. `oracle/occupancy.py` says so at the top rather than leaving the claim to stand.

**Contracts §0 and Appendix C's C18 survive, and by a stronger argument than before.** Both rest on `N_occ(d)` being solvable for through the published `theta_target_marks` and being exactly what counting a full-extent depth-*d* request's non-empty tiles already returns. Under a sketch the solved-for quantity is an *estimate* of a number §7.1 discloses exactly, so the client learns strictly less than it could learn in one call. Decision 0023's test is met a fortiori.

**§7.2's "tightening at every depth, never a loosening" survives** because the ladder clamps each rung to `4^d` before anything else. `N_occ(d) <= 4^d` still holds exactly, so no viewer is served more marks than the `4^d` progression gave, which is what that paragraph claims.

## What was not measured

* **The request path.** Every figure here is `occupancy_sketch`, not `/v1/viewport`. The `theta_occupancy_ns` trailer figure for the exact walk (1.55–2.21 ms first request at a depth, 1–3 µs after) has not been retaken on this arm.
* **The adaptive fill policy as code.** The engine's ladder fills `0..=d` unconditionally and the memo keeps every rung; the "one sketch per depth" bound is measured as a separate route rather than produced by that policy. Implementing the policy would land a session between the two bounds rather than at the naive column.
* **Corpora beyond two.** `treeoflife-1m` and `geonames`. Nothing at 2.33 × 10⁸ or 3.65 × 10⁹ rows.
* **Contention.** The box carried other work throughout — two running deployments, another session's test binary at 130% CPU, and this branch's own `cargo test --workspace` during the `geonames` interleave, contiguous and sparse runs. The harness takes the minimum of three calls and prices every route against the same mask in the same process, so the *ratios* are sound; the absolute milliseconds are upper bounds, and the three `geonames` runs above are the most affected. Two runs of the same one-segment configuration (`tol-flush` and `tol-contiguous`, where one segment is the same layout) differed by 15%.
* **Precision below 12 or above 14.** Only those two were swept.
