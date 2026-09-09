# θ's anchor: occupied tiles against `4^d`

Status: measurements taken 2026-09-08/09, supporting [decision 0137](../../docs/decisions/0137-theta-is-anchored-on-the-occupied-tile-count.md) and architecture §7.2 r62. **Not normative.** Re-take a figure before relying on it — see *What still needs measuring*.

## What was asked

§7.2 anchored the selection threshold as `θ_d = m_target · 4^d / V_total`. The `4^d` assumes a viewer's visible items spread over about `4^d` occupied tiles at depth *d*. The question was whether that holds, and what it costs when it does not.

## Method

Everything was taken through `/v1/viewport` against a running server, so every figure is the service's own answer rather than a reimplementation.

**The occupancy ladder.** A depth-9 full-extent request returns one row per occupied tile, so `N_occ(d)` for `d ≤ 9` is the number of distinct depth-`d` Morton prefixes among those tiles. Below depth 9 the request carries an explicit `tiles` list — every child of the previous level's occupied set — which is exact and costs one request per level until the list exceeds `max_tiles_per_request` (262,144).

**The effect of the new anchor, without running the new anchor.** `served(T)` is a `tessera_id`-order prefix, so the set the occupied-tile anchor would serve is recoverable exactly from a response taken under the old one: request a tile at `k = 500`, count how many of the returned identities fall below `P_E = m_target · N_occ(d) · 2⁶⁴ / V_total`, and that count *is* `C_θ` under the new anchor. If the whole prefix falls below `P_E` the tile is capped under both. This is a reconstruction, not a model — there is no fitted quantity in it — but it is **not** a measurement of the implementation.

## Figures

Corpus `treeoflife`, 2.33 × 10⁸ rows, view `bioclip`, `m_target` = 16, `k_max_marks` = 500, `k_min` = 2.

`N_occ(d)` against `4^d`, and the mean marks per occupied tile the old anchor produced:

| depth | `N_occ` (full) | `4^d` | mean marks/tile |
|---|---|---|---|
| 0 | 1 | 1 | 16 |
| 3 | 22 | 64 | 47 |
| 6 | 121 | 4,096 | 542 — capped |
| 9 | 3,025 | 262,144 | 1,387 — capped |
| 12 | 117,429 | 16,777,216 | 2,286 — capped |

The cap binds from depth 6 for the full principal, depth 5 for a 9.3% principal and depth 4 for a 0.9% one — the sparsest principal saturates earliest. At depth 12, θ had reached **1.15**: saturated, so the threshold clause was inert and `m(T) = min(cap, |vis(T)|)` unconditionally.

A 16 × 16-tile view at depth 12 over the densest region, before and after. The *after* column is
**measured against the merged implementation** (2026-09-09, server on the rebuilt 2.33 × 10⁸-row
bundle); the *before* column is exact rather than re-measured, since under the `4^d` anchor every
tile served exactly the cap:

| | old anchor | occupied-tile anchor |
|---|---|---|
| tiles at the cap | 289 / 289 | 25 / 289 |
| spread of sampling rates, all 289 tiles | 48.9× | 5.19× |
| spread over the 264 uncapped tiles | — | 1.77× |
| marks served | 144,500 | 60,002 |

True visible counts across those 289 tiles span 5,159 to 252,262. **Quote the 1.77× only against the
uncapped subset**: an earlier draft of this probe compared it against the 48.9× all-tiles figure,
which is not like for like. The capped tail is what separates them, and it is the tail the cap is
there to hold.

The reconstruction described above predicted the capped count (25 of 289) and the marks served
(60,002) exactly, which is the check that it was a sound method rather than a lucky one.

Whole-world at depth 8 the new anchor lands at 16.4 / 16.2 / 17.5 mean marks per occupied tile for the 0.9% / 9.3% / full principals against a target of 16, so the anchor calibrates. Nesting was checked by reconstruction over depths 10 → 12: **14,122 marks, 0 popped out**.

**The cost of the correction** is floor pressure at wide views. At depth 8 whole-world, tiles sitting on the `k_min` floor go from 8–29% under the old anchor to 47–66% under the new one: tightening θ corrects the dense core and the sparse tail then lands on the floor. `m_target` is a weak lever against it (16 → 128 moves the full principal only 66% → 46%). At depths 10 and 12 floor pressure measured 0–1%, so the answer is that the client descends — which the new anchor is what makes affordable.

Growth of `N_occ` per level, at full coverage, across four corpora (`treeoflife` 2.33 × 10⁸, `treeoflife-1m`, `geonames`, `medcpt-10m-abs`): **1.64 to 4.00**. The full 4× holds only where the data is genuinely space-filling — `geonames` to depth 3, `treeoflife-1m` at depth 1 — and every corpus falls below it from depth 4 down. Where `N_occ(d) = 4^d` the new anchor reproduces the old one exactly, which is why no second sampling strategy is needed.

## Route cost

Five routes for computing `N_occ(d)` inside the mask were profiled on `treeoflife-1m`, `geonames` and `medcpt-10m-abs`. The exact run-walk that gallops between occupied tile boundaries won and is what shipped; a descent using `count_range > 0` as an emptiness test was 1,600–3,400× the projection build, and extrapolating from a truncated exact prefix errs to +950% because `N_occ` saturates toward cardinality on smaller corpora.

**Two corrections worth keeping**, because both premises are plausible enough to be re-derived:

- Counting `tile(end) − tile(start) + 1` per run is **wrong**. It counts tile indices a run spans, not occupied tiles inside it — a run's Morton codes are not dense in tile-index space. Wrong by 1.6×–225× at depth 12. The gallop is what makes the walk exact.
- The descent's cost is the per-node Morton binary search and the node count, **not** `EffectiveMask::count_range`. That method is three `Bitmap::range_cardinality` calls, and CRoaring's `roaring_bitmap_range_cardinality_closed` binary-searches to the first container in range then iterates to `maxhb` and breaks — it is O(containers in the range), exactly as `compose.rs` documents. An earlier draft of this probe asserted otherwise; it was wrong.

The bench binary that produced the route tables was left uncommitted in the `probe/nocc-cost` worktree and is not preserved here.

## Cost at 2.33 × 10⁸ rows

Measured 2026-09-09 against the merged implementation, as the wall-clock delta between a cold and a
warm request at the same depth and bbox — so an **upper bound** on the walk including any other
per-depth cold state, not the `theta_occupancy_ns` stage timer, which needs a `bench-timing` build.

| depth | 4 | 6 | 8 | 10 | 12 | 14 | 16 |
|---|---|---|---|---|---|---|---|
| full principal | 36 | 33 | 37 | 35 | 53 | 183 | 165 |
| 0.9% principal | 38 | 40 | 83 | 36 | 36 | 38 | 38 |

Milliseconds, one-off per `(session, depth)` behind the memo. Against 1.6–2.2 ms on `treeoflife-1m`
this is sublinear in a 233× larger corpus: **the concern that a ~932 MB Morton column would punish
the walk's random access did not materialise.** A first attempt put depth 9 at ~827 ms; that was an
artefact of comparing a cold and a warm request whose responses differed in size, and is not the
walk.

## What still needs measuring

- **The multi-segment arm.** Every figure here comes from single-segment corpora, which take a
  counter path that allocates nothing. Under [decision 0091](../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md)
  a live view is multi-segment, so the arm that builds a Roaring union over tile indices — bounded
  at ~512 MB at depth 16 — is the deployed one and has no measurement behind it.
- **The `theta_occupancy_ns` stage timer**, which would separate the walk from the rest of a cold
  request rather than bounding it.
- The driver scripts were not preserved. The occupancy ladder no longer needs them: `Engine::occupied_tiles_for_test`
  (feature `fault-injection`) and the `theta_occupancy_ns` trailer field are a better route.
