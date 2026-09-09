# 0138 — `N_occ` is a sketch above one segment, and its ladder is filled in the background

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

**The ladder is filled to depth 12 in the background**, by a task the first request that took θ's
anchor spawns. Above 12 it is extended lazily, on demand, as before. Filling it at *authorise*
instead — which is what would take the walk off a session's very first request — was implemented and
measured and is **not landed**; the section below says why, and it is a ruling rather than an
implementation choice.

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
ladder filled off the request path the walk's own speed stopped being the thing to buy, and what
2¹² spends to get it is the quantity θ is judged on: double the error, and six times as many rungs
where the running maximum has to hold a fallen estimate up rather than serve the estimator's own
answer. θ is linear in `N_occ`, so 2.041% moves the mean occupied tile from 16 marks to 15.67 and
4.082% moves it to 15.35 — neither is visible, which is the point: nothing is bought by the
looser one either.

## The ladder is filled to depth 12 in the background

**A request that took θ's anchor spawns the fill**, carrying the geometry it already resolved. The
request walks once for its own depth and keeps every rung below it; the fill then walks once more
and takes the ladder to 12. One walk per `(session, view, generation)` for every depth a client is
likely to visit, where before there was one per depth.

**Depth 12 rather than 16.** Measured exactly off the stored Morton column, independently of the
engine's walk: on `treeoflife` at 2.33 × 10⁸ rows `N_occ(12)` is 117,429 against `N_occ(16)`'s
8,330,745, so the four deepest rungs are **76 times** the rest of the ladder in emissions and 3
times it in walk time (63 ms at depth 12 against 191 ms at 16). On `geonames`, 1,399,919 against
11,543,951. A client's depth is bounded by its own mark budget rather than by the grid — the tile
count a request carries is `B / m_target` independently of zoom
(`clients/ts/core/src/budget.ts`) — so it rarely reaches past 12–13. **Above 12 the ladder is
extended lazily, from a request**, as before.

**It runs on the pool the engine already has**, one fill per session at a time so a client panning
at ten frames a second queues one walk rather than ten, and it is cancelled and dropped by
`Engine::prune_token` at revoke. It touches no cache but the occupancy memo: it carries its
request's `SessionGeometry`, so it looks nothing up and publishes nothing, which is what keeps it
invisible to `Engine::session_geometry`'s three-rung ladder and to `crate::refresh`.

`Engine::occupancy_walks` is a new operator gauge counting every walk, from a request or from a
fill. **The number to watch is walks per session per publication**, and it should be one per view.

### Filling it at authorise instead: measured, and not landed

The brief for this work asked for the ladder to be filled at authorise, on the premise that a
session's row projection is already built there. **It is not** — `Engine::authorise` resolves the
credential and builds the mask fragment, and the row projection is built by the session's first
request. Since `N_occ` must be counted over the **composed** mask (**I2**), and the composed mask
needs the projection, filling the ladder at authorise means building the projection at authorise.

That was implemented and measured. It works: at 2.33 × 10⁸ rows a cold session's first request at
depth 3 falls from **840 ms to 285 ms** — 285 ms being what the same request costs warm — because
the projection build leaves the request path with the walk. It needs about a second of gap between
authorise and the first viewport, which a browser leaves and a load generator does not.

**It is not landed, because it changes three things this brief did not put in front of the owner.**

- **Decision 0044's rung-2 stale serve reaches a session's first request.** The stage leaves a
  resident row-projection entry at the generation it ran under; after one flush the session's first
  ever request is served from it rather than building at the live generation, so it can miss the
  items of the flush that just happened. That is sound and self-correcting within one refresh round
  — 0044 accepts exactly this for established sessions — but extending it to *establishment* is a
  freshness change, and seven ingest and join tests assert the freshness it removes.
- **`Engine::authorise` can be refused `FragmentBuilding` by background work.** The stage brings a
  fragment forward when the generation has moved past the session's own, and a concurrent authorise
  on the same canonical key is then refused rather than blocked. That refusal already exists, but
  it used to mean *another caller wants this*.
- **A session that authorises and never views pays a full `Permutation::project` per visible view**,
  and under a row-projection bound that holds fewer entries than there are sessions the stage
  builds entries the bound evicts before their session reads them.

The figures, the sequence that produces each, and how to re-take them are in
`probes/2026-09-09-nocc-stage/README.md`. **The ruling is the owner's.**

## What it changed, measured

`treeoflife`, 2.33 × 10⁸ rows, `bioclip`, full-coverage principal (all 474 `publisher` terms), a
1,024-tile window centred on the densest depth-6 tile, response-trailer stage timings, 2026-09-09.
**One session zooming in from depth 3 to 16 and back out**, which is what the fill is for. `walk`
is `theta_occupancy_ns`; medians of three runs without the fill and of two with it. The box carried
another session's `tessera build` throughout, so these are upper bounds; before and after were taken
minutes apart on it.

| depth | walk, before | walk, after |
|---|---|---|
| 3 (the session's first request) | 36.9 ms | 36.0 ms |
| 4 | 35.3 ms | **0** |
| 6 | 36.2 ms | **0** |
| 8 | 39.8 ms | **0** |
| 10 | 39.0 ms | **0** |
| 12 | 67.5 ms | **0** |
| 13 | 99.2 ms | 108.8 ms |
| 16 | 201.0 ms | 269.1 ms |

**Zooming from 3 to 12 costs one walk instead of six: 36 ms against 255 ms, a 219 ms saving.** A
depth-12 request falls from 188 ms to 128 ms in total. Depths 13 and 16 are above the ceiling and walk on
demand, unchanged. Zooming back out was already free — one walk fills every rung below it — and
still is.

**The walk was never the largest term in a cold first paint at this scale.** Before any of this, a
cold first request at depth 3 spent 592 ms waiting for the row projection, 33 ms on the walk and
216 ms on the tile sweep. The walk is 4% of a cold request at depth 3 and 24% at depth 16 — but 13%
of what the same request costs *warm* at depth 3 and 95% at depth 16, which is the comparison that
matters for every request after the first.

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
