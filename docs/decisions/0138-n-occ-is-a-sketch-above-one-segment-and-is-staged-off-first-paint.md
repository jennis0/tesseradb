# 0138 — `N_occ` is a sketch above one segment, and its ladder is staged off first paint

**Date:** 2026-09-09 · **Status:** Settled (owner ruling) · Built; design
[`architecture.md`](../design/architecture.md) §7.2 r63, [`contracts.md`](../design/contracts.md)
r92, `crates/tessera-engine/src/occupancy.rs`, `crates/tessera-engine/src/stage.rs`,
`reference/oracle/occupancy.py`. Amends [decision 0137](0137-theta-is-anchored-on-the-occupied-tile-count.md).
Evidence: [`probes/2026-09-09-nocc-sketch/`](../../probes/2026-09-09-nocc-sketch/README.md),
[`probes/2026-09-09-nocc-stage/`](../../probes/2026-09-09-nocc-stage/README.md).

## What this answers

Decision 0137 made θ's second anchor the occupied-tile count `N_occ(d)`, counted exactly. Two
things followed that it did not settle.

**Counting it exactly across segments needs an accumulator, and three branches were written to
choose one.** A tile can hold rows in several segments, so a per-segment walk cannot simply count
its own emissions; the shipped arm unioned tile indices into a Roaring bitmap, and two branches
existed to beat it — a sort, and a direct-mapped bitset to depth 12 with a compacting sorted buffer
below. Three arms, a tuning constant and a time-against-memory trade, for a scalar.

**It was paid on the first request at each new depth.** `N_occ` is memoised on
`(token_id, view, depth, segments_version, overlay_version, fragment identity, fragment watermark)`,
so a session's first request at a depth pays the whole walk — a measured 33–213 ms at 2.33 × 10⁸
rows. Nothing requires it to be paid there: the walk is a function of the composed mask, the view
and the depth, and of nothing a request supplies.

## The decision

**One segment counts `N_occ(d)`. Two or more estimate it, with a HyperLogLog.** The predicate is
`segments.len() == 1` and it is the whole of the route choice.

**The ladder is filled at authorise, in the background, to depth 6 and then to depth 12.** Above 12
it is extended lazily, on demand, as before.

### Why one segment counts

Within one segment the walk emits each occupied tile once and ascending, so the ancestor-changed
test fires exactly `N_occ(d')` times at each depth and **a counter is the accumulator**. There is
nothing for a sketch to buy and a hash to pay: measured over all seventeen rungs at one segment,
12.5 ms counted against 24.8 ms sketched over 10⁶ rows and 129 ms against 189 ms over
1.35 × 10⁷, and across the eight one-segment cells swept the sketch lost at every one, by 0.61× to
0.88×.

`segments == 1` is also the natural boundary rather than a tuned one: the union, the sort and the
bitset only ever existed for the multi-segment case, so this is the predicate that was always
there, made explicit. A freshly built bundle has one segment per view, and so does every
conformance fixture and every demo corpus; a live deployment accumulates one segment per flush and
is estimating within a publication or two of opening.

### Why two or more estimate

Above one segment the counter is not available at all — counting ancestor changes per segment
counts a shared tile once per segment that holds it — which is what the three accumulators existed
to fix. A sketch merges by taking the maximum per register, and that **is** the union: adding a
tile twice is adding it once, so one sketch fed by every segment needs no accumulator at all and
the multi-segment case stops being a special case.

What it buys is memory that does not move with the corpus. The register plane is
`(depth + 1) · 2^precision` bytes and nothing else in the arm grows: a measured peak of **278,528
bytes for a whole depth-16 ladder, identical at every segment count and on both corpora swept**
(10⁶ and 1.35 × 10⁷ rows). Against it, over the same sweep, the Roaring union reached 3.35 MB and
22.1 MB, the bitset arm 8.19 MB and 92.6 MB, and seventeen exact accumulators behind the same walk
23.6 MB and 264 MB. The counted route's own state is two `[u64; 17]` arrays — 272 bytes — so
neither route holds anything that scales.

### What it costs: a single tile stops being observable above one segment

`N_occ` was exact, so a suppression that emptied one tile lowered it by exactly one and a test could
assert that. That assertion survives on the counted route and
`n_occ_falls_when_a_suppression_empties_a_tile` still makes it. Above one segment it cannot be made
at any interesting scale: at a million occupied tiles a single emptied tile is far inside the
estimator's error. **The property is unchanged** — the anchor is composed rather than pre-overlay,
so a deny moves it — and only the resolution at which it can be asserted moves with the route.

### θ changes character across a flush, and that is accepted

A view's first flush takes it from counted to estimated. θ moves on a flush regardless — `V_total`
moves, and `N_occ` itself moves as new rows occupy new tiles — and the memo is keyed on
`segments_version`, so no session is ever served the two routes' answers for one generation.

## The prohibition on a running maximum is deleted

Design §7.2 r62 read: *"No implementation may clamp θ or carry a running maximum over depth: a
clamp would conceal a miscount rather than prevent one."* That is right about an exact count, where
a fall in `N_occ` between adjacent depths can only be a bug. It is the reverse under an estimate:
where `N_occ` grows by less than the estimator's error, two adjacent estimates can land either side
of it and invert, and nothing about the walk is wrong.

Measured, over 9,792 swept cells: **sixteen inversions before the running maximum and none after**.
They are all the same cell at every segment count — `treeoflife-1m` under a 5% mask, depths 14 to
15, where the raw estimate falls 50,352 → 50,187, a 0.33% step under a 0.81% standard error.
Without the maximum θ would shrink on that zoom step and the child tile would draw fewer marks than
its parent, which is what §7.2's nesting proof forbids.

**Owner ruling (2026-09-09): lose the sentence rather than qualify it.** §7.2 r63 now states the
running maximum as the source of the property, applied after the `4^d` clamp. Both steps can only
raise a rung — serving a few more marks than `m_target` asks is harmless where serving fewer breaks
nesting — and neither binds on the counted route, where they are applied anyway so that one tail
establishes the property for both routes.

The miscount r62 feared is caught where it was always caught: by the conformance differential, and
by `the_occupied_tile_count_is_exact_monotone_and_bounded` and
`the_sketch_route_estimates_the_tile_set_a_linear_scan_finds`, which pin the walk's emissions
against a linear scan that shares no code with it.

## The register count: 2¹⁴ kept, 2¹² declined

Both were swept over the same 9,792 cells.

| | 2¹⁴ (16 kB a rung) | 2¹² (4 kB a rung) |
|---|---|---|
| median \|relative error\| | **0.323%** | 0.772% |
| p90 | **1.562%** | 2.768% |
| max | **2.041%** | 4.082% |
| inversions before the running maximum | **16** | 96 |
| inversions after it | 0 | 0 |
| register plane, depth-16 ladder | 278,528 B | **69,632 B** |
| deepest-rung walk, median of 56 multi-segment cells | 32.23 ms | **30.69 ms** (0.913×) |

2¹² is 9% faster at 45 of those 56 cells, which is the cache argument holding: about 1.5 rungs are
touched per emitted tile and 68 kB sits closer than 272 kB. **It is declined anyway.** With the
ladder staged off the request path the walk's own speed stopped being the thing to buy, and what
2¹² spends to get it is the quantity θ is judged on: double the error, and six times as many rungs
where the running maximum has to hold a fallen estimate up rather than serve the estimator's own
answer. θ is linear in `N_occ`, so 2.041% moves the mean occupied tile from 16 marks to 15.67 and
4.082% moves it to 15.35 — neither is visible, which is the point: nothing is bought by the
looser one either.

## The ladder is staged at authorise

**Rungs 0..=6 first, then 0..=12.** The client's first request falls back to `budget.ts`'s
average model and starts shallow (`MIN_DEPTH` is 3), so the shallow rungs are the ones first paint
reads and they are the cheap end: a walk at depth 6 emits at most `4^6` tiles. The second stage
subsumes the first and costs almost nothing extra, because one walk at depth *d* fills every rung
at or below it — a shallow walk is dominated by the deep one that follows — and what the split
buys is that the shallow rungs land early rather than at the end of the deep walk.

**Depth 12 rather than 16.** Measured exactly, off the stored Morton column and independently of
the engine's walk: on `treeoflife` at 2.33 × 10⁸ rows `N_occ(12)` is 117,429 against `N_occ(16)`'s
8,330,745, so the four deepest rungs are **76 times** the rest of the ladder in accumulator work
and 3 times it in walk time (a measured 63 ms at depth 12 against 191 ms at 16). On `geonames`,
1,399,919 against 11,543,951. A budget-limited client rarely reaches past 12–13, so staging to 16
would spend that to cover the depths fewest sessions reach.

**It runs on the pool the engine already has**, as one task per authorise, and is cancelled and
dropped by `Engine::prune_token` at revoke. It never waits on the row-projection cache — that would
park a rayon worker on work that needs rayon workers — and a key another caller is already building
is skipped rather than duplicated.

### Staging the ladder stages the row projection, and that is the substantive change

`N_occ` must be counted over the composed `EffectiveMask` (**I2**), so the stage has to resolve the
session's row projection first; on a cold session nothing else has built it, so the stage is the
builder. **It cannot ride on the projection instead.** The projection is `M_auth` *before* the
overlay diff and a HyperLogLog cannot subtract, so a sketch filled there could never have a
suppressed tile removed from it and a deny that emptied a tile would leave `N_occ` where it was —
exactly the I2 property `n_occ_falls_when_a_suppression_empties_a_tile` pins.

So this moves session establishment — a full `Permutation::project`, decision 0044's accepted
"not update-induced" cost — from a session's first request to its authorise. Four consequences,
each of them real:

- **For a session that views, it is the first request's own work started earlier**, and it is
  counted in `Engine::full_projection_builds` beside the request path's so the operator gauge does
  not under-report.
- **For a session that authorises and never views, it is speculative.** That is why the task is
  cancellable and why nothing on a request's critical path waits for it.
- **A session that can reach *V* views stages all of them**, because nothing at authorise says
  which it will ask for. That is `V − 1` extra resident projections for a session that views one.
  A `view` hint on `/session/authorise` would remove the multiplier and is not designed.
- **Under a row-projection bound that holds fewer entries than there are sessions**, the stage
  builds entries the bound then evicts before their session reads them. That is wasted pool time,
  not a wrong answer, and it is the case a deployment sizing that bound has to keep in view.

### What it does not cover

The rungs go stale on **every publication**, because the memo key carries `segments_version`,
`overlay_version` and the fragment's identity and watermark. Under continuous ingest that is the
dominant case, and warming from `crate::refresh` — which already rebuilds each resident session's
projection per publication — would cover it and be worth more than this. It is not built: the
refresh pass is a measured ~0.7 s round whose shed margin is already under 2× (decision 0044,
`probes/2026-08-14-project-decomposition/`), and adding a walk per entry is a change to that budget
rather than to this one.

## What it changed, measured

`treeoflife`, 2.33 × 10⁸ rows, `bioclip`, full-coverage principal (all 474 `publisher` terms), a
1,024-tile window centred on the densest depth-6 tile, medians over reps, response-trailer stage
timings, 2026-09-09. "Gap" is the delay between `/session/authorise` returning and the first
`/v1/viewport` — a browser leaves one, because it fetches `/v1/meta` and initialises a map in it.

| depth | before, 1 s gap | after, 1 s gap | of which the walk, before |
|---|---|---|---|
| 3 | 840 ms | **285 ms** | 33 ms |
| 6 | 928 ms | **285 ms** | 36 ms |
| 12 | 834 ms | **142 ms** | 63 ms |
| 13 | 800 ms | 189 ms | 92 ms |
| 16 | 845 ms | 231 ms | 191 ms |

At depths 3 to 12 the cold first request pays **no** projection wait and **no** walk: 285 ms is
what the same request costs warm. Depths 13 and 16 are above the staged ceiling and still pay the
walk lazily, by design.

**The gap has to be about a second at this scale.** At a 0.5 s gap the first request still waited a
median 156 ms for the projection and still paid the walk; at 1.0 s it paid neither. At a **zero**
gap the stage buys nothing on the first request — it waits on the projection the stage is building,
which is the same wall time it would have spent building it — and everything on the second.

## What is not covered, and what the differential now compares

Every bundle the reference oracle opens has exactly one segment per view
(`reference/oracle/bundle.py`), so `Selection.n_occ` takes the counted route and
`conformance/tests/test_i7_selection.py` compares two exact counts as it always did.
`reference/oracle/occupancy.py` carries both routes behind the engine's own predicate, and the
estimator's cross-language pin is a vector test asserted on both sides
(`the_ladder_matches_the_python_oracle_vector_for_vector` and
`test_the_ladder_matches_the_engine_vector_for_vector`) rather than the differential. **That is a
real limit on the suite's coverage of the estimator** and is stated in both files rather than left
to be found.
