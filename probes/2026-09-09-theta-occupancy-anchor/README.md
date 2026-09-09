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

A 16 × 16-tile view at depth 12 over the densest region, before and after:

| | old anchor | occupied-tile anchor |
|---|---|---|
| tiles at the cap | 289 / 289 | 25 / 289 |
| spread of per-tile sampling rates | 49× | 1.77× |
| marks served | 144,500 | 60,002 |

Whole-world at depth 8 the new anchor lands at 16.4 / 16.2 / 17.5 mean marks per occupied tile for the 0.9% / 9.3% / full principals against a target of 16, so the anchor calibrates. Nesting was checked by reconstruction over depths 10 → 12: **14,122 marks, 0 popped out**.

**The cost of the correction** is floor pressure at wide views. At depth 8 whole-world, tiles sitting on the `k_min` floor go from 8–29% under the old anchor to 47–66% under the new one: tightening θ corrects the dense core and the sparse tail then lands on the floor. `m_target` is a weak lever against it (16 → 128 moves the full principal only 66% → 46%). At depths 10 and 12 floor pressure measured 0–1%, so the answer is that the client descends — which the new anchor is what makes affordable.

Growth of `N_occ` per level, at full coverage, across four corpora (`treeoflife` 2.33 × 10⁸, `treeoflife-1m`, `geonames`, `medcpt-10m-abs`): **1.64 to 4.00**. The full 4× holds only where the data is genuinely space-filling — `geonames` to depth 3, `treeoflife-1m` at depth 1 — and every corpus falls below it from depth 4 down. Where `N_occ(d) = 4^d` the new anchor reproduces the old one exactly, which is why no second sampling strategy is needed.

## Route cost

Five routes for computing `N_occ(d)` inside the mask were profiled on `treeoflife-1m`, `geonames` and `medcpt-10m-abs`. The exact run-walk that gallops between occupied tile boundaries won and is what shipped; a descent using `count_range > 0` as an emptiness test was 1,600–3,400× the projection build, and extrapolating from a truncated exact prefix errs to +950% because `N_occ` saturates toward cardinality on smaller corpora.

**Two corrections worth keeping**, because both premises are plausible enough to be re-derived:

- Counting `tile(end) − tile(start) + 1` per run is **wrong**. It counts tile indices a run spans, not occupied tiles inside it — a run's Morton codes are not dense in tile-index space. Wrong by 1.6×–225× at depth 12. The gallop is what makes the walk exact.
- The descent's cost is the per-node Morton binary search and the node count, **not** `EffectiveMask::count_range`. That method is three `Bitmap::range_cardinality` calls, and CRoaring's `roaring_bitmap_range_cardinality_closed` binary-searches to the first container in range then iterates to `maxhb` and breaks — it is O(containers in the range), exactly as `compose.rs` documents. An earlier draft of this probe asserted otherwise; it was wrong.

The bench binary that produced the route tables was left uncommitted in the `probe/nocc-cost` worktree and is not preserved here.

## What still needs measuring

- **This implementation's own cost at 2.33 × 10⁸ rows.** The in-situ figures in decision 0137 are `treeoflife-1m` only (walk 1.6–2.2 ms cold, 1.2–2.9 µs warm behind the memo, against a 14–157 ms request). Every route figure was taken with the Morton column resident — 4–54 MB on those corpora, ~932 MB at 2.33 × 10⁸ — and the walk is random-access where the linear oracle is sequential, so the ranking between them may invert on a cold column. Nothing here carries across.
- **The before/after pair in the table above** was reconstructed, not run. Re-take it against the implementation once a server is up on the rebuilt corpus.
- The driver scripts were not preserved. The occupancy ladder no longer needs them: the branch adds `Engine::occupied_tiles_for_test` (feature `fault-injection`) and a `theta_occupancy_ns` trailer field, which is a better route to the same numbers.
- `data/ladder/treeoflife` was rebuilt on 2026-09-08/09, so these figures describe the previous build of that corpus.
