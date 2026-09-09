# `N_occ`'s ladder, off the request path

Status: measurements taken 2026-09-09 on `perf/nocc-sketch-land`, against `data/ladder/treeoflife`
(2.33 × 10⁸ rows, one segment per view). **Not normative.** [Decision
0138](../../docs/decisions/0138-n-occ-is-a-sketch-above-one-segment-and-its-ladder-is-filled-in-the-background.md)
is what this supports; re-run a figure before relying on it. The 3.65 × 10⁹ section is **arithmetic
over measured scaling, not a measurement** — it says so at every number, and
[Re-running against GBIF](#re-running-against-gbif) is how to replace it with one.

## What was asked

θ's second anchor `N_occ(d)` was memoised per depth and therefore paid on the first request at each
new depth — a measured 33–213 ms at 2.33 × 10⁸ rows. Two questions: what a first paint actually
spends, and what moving the walk off a request buys.

**Two placements were measured.** The one that landed fills the ladder from a *request*, which
covers every depth after a session's first. The one that did not fills it at *authorise*, which
covers the first as well — and, because `N_occ` needs the composed mask and so the row projection,
takes the projection build off the first request too. It is not landed for the three reasons under
[Filling at authorise](#filling-at-authorise-measured-and-not-landed), each of which is a ruling
rather than an implementation choice.

## Method

`first_paint.py` drives the shipped server over HTTP and reads the response trailer's `stage_ns`
CSV (`serve.stage_timing = true`, a `bench-timing` build). **One fresh `/session/authorise` per
row**, so every row is a session the process has never seen: a cold row projection and a cold
ladder, which is what a first paint is. `/v1/meta` is fetched in between because a browser fetches
it in between, and `--settle` adds the rest of whatever gap a client leaves before its first
viewport.

The window is **centred on the densest depth-6 tile and 32 × 32 tiles wide** — 1,024, the order a
client at a 5 × 10⁴-mark budget carries (`B / m_target`, `clients/ts/core/src/budget.ts`). `N_occ`
is viewport-invariant, so the window cannot move `theta_occupancy_ns`; it is sized so that the
*rest* of the request is one a client would actually make, since a full-extent request at depth 10
spans `4^10` tiles and is refused, and a window over empty extent times a sweep over no rows. The
run warms the page cache at every depth it will measure before timing anything.

Principal: all 474 keys of `publisher`, `treeoflife`'s access column — full coverage. View
`bioclip`. Medians over reps. **The box carried another session's `tessera build` throughout**
(load average 2.9–4.7), so absolute milliseconds are upper bounds; before and after were taken on
the same box within minutes of each other, so the difference is sound.

`n_occ_ladder.py` counts `N_occ(d)` exactly off a segment's `morton.u32` with a `numpy` diff —
the column is in Morton order, so the tile index is non-decreasing along it and the distinct count
is one plus the number of steps. It shares no code with the engine's walk or with the sketch.

## What landed: one walk per session, not one per depth

`treeoflife`, `bioclip`, full coverage, the window above. **One session zooming in from depth 3 to
16 and back out** — what a viewer does, and what a request-triggered fill covers. `walk` is
`theta_occupancy_ns`; medians of three runs with the fill off and two with it on.

| depth | walk, fill off | walk, fill on | total, fill off | total, fill on |
|---|---|---|---|---|
| 3 *(the session's first request)* | 36 | 36 | 1 290 | 2 954 † |
| 4 | 35 | **0** | 266 | 240 |
| 6 | 36 | **0** | 265 | 228 |
| 8 | 40 | **0** | 339 | 283 |
| 10 | 39 | **0** | 400 | 369 |
| 12 | 68 | **0** | 188 | 121 |
| 13 | 99 | 92 | 167 | 160 |
| 16 | 201 | 209 | 212 | 221 |

† the first request also builds the row projection, which is 0.96–2.7 s here and is not what this
row is about; the walk beside it is.

**Zooming from 3 to 12 costs one walk instead of six: 36 ms against 218 ms.** Depths 13 and 16 are
above `stage::BACKGROUND_DEPTH` and walk on demand, unchanged. Zooming back out was already free —
one walk fills every rung below it — and still is. The runs are in `trajectory-filled.json` and
`trajectory-unfilled.json` beside this file.

## Filling at authorise: measured, and not landed

Filling the ladder at authorise takes the walk off a session's **first** request as well. Because
`N_occ` must be counted over the composed mask, doing so means building the row projection at
authorise — which takes that off the first request too.

Milliseconds, medians, one fresh session per row. *Before* is the same binary with the
authorise-time stage off (3 reps); *after* is with it on (5 reps). "Gap" is the delay between
authorise returning and the first viewport.

| depth | before, 1 s gap | after, 1 s gap | before: projection wait | before: walk |
|---|---|---|---|---|
| 3 | 840 | **285** | 592 | 33 |
| 4 | 933 | **270** | 641 | 35 |
| 6 | 928 | **285** | 639 | 36 |
| 8 | 955 | **298** | 616 | 38 |
| 10 | 1 096 | **400** | 659 | 43 |
| 12 | 834 | **142** | 652 | 63 |
| 13 | 800 | 189 | 640 | 92 |
| 16 | 845 | 231 | 644 | 191 |

**At depths 3 to 12 the cold first request pays neither.** Its `row_projection_ns` and
`theta_occupancy_ns` are both zero and its total is what the *second* request at the same depth
costs — 285 ms against a warm 313 ms at depth 3, inside the run-to-run spread.

**The gap has to be about a second at this scale**, for `treeoflife`'s two views:

| gap | projection wait | walk |
|---|---|---|
| 0 s | 685–845 ms | 47–230 ms |
| 0.5 s | 149–157 ms | 39–67 ms |
| 1 s | **0** | **0** (to depth 12) |
| 2, 3, 5 s | 0 | 0 (to depth 12) |

At a zero gap it buys nothing on the first request: the request arrives while the projection is
still building and waits for it through the single flight — the same wall time it would have spent
building it itself. A browser leaves a gap of roughly this order; a load generator that authorises
and immediately requests does not, and will measure the zero-gap row.

### Why it is not landed

**Decision 0044's rung-2 stale serve reaches a session's first request.** The stage leaves a
resident row-projection entry at the generation it ran under, so after one flush a session's first
ever request is served from it rather than built at the live generation, and can miss the items of
the flush that just happened. That is sound and self-corrects within one refresh round — 0044
accepts exactly this for established sessions — but extending it to *establishment* is a freshness
change. Seven ingest and join tests assert the freshness it removes; `views_write`'s
`a_known_external_id_joins_a_second_view_and_is_placed_in_each` is the clearest: it authorises,
ingests into a second view, flushes, and reads, and under the stage it reads the pre-flush row
space.

**`Engine::authorise` can be refused `FragmentBuilding` by background work.** The stage brings a
fragment forward when the generation has moved past the session's own, and a concurrent authorise
on the same canonical key is refused rather than blocked. That refusal already exists; it used to
mean *another caller wants this*.

**A session that authorises and never views pays a full `Permutation::project` per visible view**,
and under a row-projection bound that holds fewer entries than there are sessions the stage builds
entries the bound evicts before their session reads them (`cache.rs`'s
`an_undersized_bound_does_not_livelock` stops evicting at all with the stage on, because its
round-robin's misses become hits).

### The walk was never the largest term in a cold first paint

Before any of this, a cold first request at depth 3 spent 592 ms waiting for the row projection,
33 ms on the walk and 216 ms on the tile sweep. The walk is 4% of a cold request at depth 3 and 24%
at depth 16 — but it is 13% of what the same request costs *warm* at depth 3 and 95% at depth 16,
which is the comparison that matters for every request after the first, and the one the landed fill
addresses.

## The ladder, exactly

`n_occ_ladder.py`, whole column, unmasked.

| depth | `treeoflife` (2.33 × 10⁸ rows) | `geonames` (1.35 × 10⁷ rows) |
|---|---|---|
| 6 | 121 | 2 503 |
| 10 | 10 410 | 168 534 |
| 12 | **117 429** | **1 399 919** |
| 13 | 363 779 | 3 405 511 |
| 16 | **8 330 745** | **11 543 951** |

**This is why the background fill stops at 12.** The four deepest rungs are 76 times the whole rest
of the ladder in emissions on `treeoflife` (8.21 × 10⁶ against 1.68 × 10⁵) and 7 times it on
`geonames`. In walk *time* the ratio is smaller — a measured 63 ms at depth 12 against 191 ms at 16
— because the walk's cost is not only its emissions; it is still 3×.

## The register count

Both precisions over the same 9,792 cells of `probes/2026-09-09-nocc-sketch`'s sweep (both corpora,
three split shapes, 1 to 512 segments, a 5% mask, every depth). Re-derived from that probe's JSON
rather than re-measured.

| | 2¹⁴ | 2¹² |
|---|---|---|
| median \|relative error\| | **0.323%** | 0.772% |
| p90 | **1.562%** | 2.768% |
| max | **2.041%** | 4.082% |
| raw inversions (before the running maximum) | **16** | 96 |
| inversions after it | 0 | 0 |
| register plane, depth-16 ladder | 278 528 B | **69 632 B** |
| depth-16 walk, median of 56 multi-segment cells | 32.23 ms | **30.69 ms** |
| per-cell time ratio 2¹²/2¹⁴ | — | median **0.913**, 45 of 56 below 1.0 |

**Recommendation: keep 2¹⁴.** 2¹² is 9% faster and the cache argument behind that holds — about
1.5 rungs are touched per emitted tile, and 68 kB sits closer than 272 kB. But the walk is now paid
once per session rather than once per depth, so 9% of it buys nothing a viewer can see, and what 2¹²
spends
to get it is θ's accuracy: double the error, and six times as many rungs where the running maximum
has to hold a fallen estimate up rather than serve the estimator's own answer. Neither error is
visible — θ is linear in `N_occ`, so 2.041% moves the mean occupied tile from 16 marks to 15.67 and
4.082% moves it to 15.35 — which cuts both ways: there is nothing to buy with the looser one.

## Projected to 3.65 × 10⁹ points

**None of this is measured.** GBIF (rung 6) does not exist yet. Every figure below is arithmetic
over the measurements above, and the assumption each rests on is named so it can be checked.

The scale factor is 3.65 × 10⁹ / 2.33 × 10⁸ = **15.66×**. The Morton column is 4 bytes a row
exactly, so it goes from 932 MB (measured file size, 932 223 944 B) to **14.6 GB**.

**The walk's floor becomes a pass over that column, at every depth from about 11 down.** A 4 KiB
page holds 1,024 codes; at 3.65 × 10⁹ rows there is at most one occupied-tile boundary every 870
rows at depth 11 and every 218 at depth 12, so essentially every page holds a boundary and the walk
touches all of them. At depth 6 the boundaries are 891 thousand rows apart and it touches almost
nothing. The depth-16 walk at 2.33 × 10⁸ moved 932 MB in a measured 191 ms — **4.89 GB/s
effective**, page cache resident — so *if that rate holds*, the pass is **≈3.0 s**. Add the
emission term, which the grid bounds at depth 12 and does not usefully bound at 16: at the ~16 ns
per emitted tile implied by the depth-12 and depth-16 measurements, `4^12` emissions is 0.27 s and
`4^16` is 69 s.

**So the background fill to depth 12 is projected at ≈3 s of pool time per session per view**,
against ~0.1 s today. That is the number to check first when GBIF lands, and it is why the cap to
move if it proves too much is 12 → 10, not 12 → 16 — depth 10 is where the walk stops touching every
page.

### Risk 1 — the sketch's memory must be flat, and is

**Structural.** The register plane is `(depth + 1) · 2^SKETCH_PRECISION` bytes and the arm allocates
nothing else; the counted route's whole state is two `[u64; 17]` arrays, 272 bytes. Neither term
mentions the row count, the segment count or the mask.

**Measured, and the measurement is the point**: the sketch ladder's peak live bytes is 278 528 at
*every* segment count from 1 to 512 and on *both* corpora, which differ by 13.5× in rows. Against
it, on `geonames`, the Roaring union reached 22.1 MB, the bitset arm 92.6 MB and seventeen exact
accumulators behind the same walk 264 MB — all three of which do grow with the data. **At
3.65 × 10⁹ the sketch ladder is still 278 528 bytes** (212 992 for the staged fill to depth 12).
That is the headline claim for this scale and it is the one thing here that needs no projection.

*To check when GBIF lands:* `occupancy_sketch` reports `ladder_peak_bytes` from a counting global
allocator. It must still read 278 528 at depth 16.

### Risk 2 — the walk is a forward scan with skips, and the code says so

**It holds.** `for_each_occupied_tile` walks the segment in ascending 2²²-row chunks;
`EffectiveMask::for_each_visible_run` yields each chunk's runs ascending; `Walk::visit_run` starts
at the run's first row and moves forward only, and `gallop` searches out from the current cursor —
probing `from+1, from+2, from+4, …` and then binary-searching inside the final bracket. Index
access is monotone non-decreasing except for the bounded backtrack inside one bracket, whose span
is the step just taken. **There is no random access and no backwards seek**, so a 14.6 GB column
degrades with readahead rather than falling off a page-fault cliff — *provided it is resident*.

Residency is the real risk, not the pattern: 14.6 GB of Morton column inside a bundle that will be
larger still, on a 47 GB box. The 4.89 GB/s above is a page-cache rate; a column read from disk at,
say, 500 MB/s makes the same pass 29 s.

**Shallower rungs probe the column far less.** A walk at depth 6 touches at most `4^6` boundaries —
a few megabytes of cache lines however large the corpus — where a walk at 12 touches every page.
That is the lever if the fill proves too expensive at 3.65 × 10⁹: the ceiling, not the register
count.

### Risk 3 — row ids are `u32` and 3.65 × 10⁹ is 85% of that space

**Nothing in this path overflows at 3.65 × 10⁹, and the margin is 15%.**

- **The ceiling is enforced upstream, at the one seam that grows a row space.**
  `Permutation::with_extent` refuses an extent that would take a view past `u32::MAX` rows
  (`crates/tessera-store/src/permutation.rs`, "Row ids are `u32` (bundle_format 1), so a view that
  would cross 2^32 rows must fail here rather than at the first `row_base + slot` that wraps"), and
  `fold_row_space` refuses the same at its own `checked_add`.
- **The occupancy walk relies on that guard and does not restate it.**
  `for_each_occupied_tile` forms `row_base + start .. row_base + end` as plain `u32` additions.
  Their largest possible value is exactly the view's total row count, so they are safe *because* of
  the upstream refusal and for no other reason. At 3.65 × 10⁹ the largest is 85% of `u32::MAX`.
- **`4^16` is exactly `2^32`, and every quantity that can reach it is `u64`.** `tile_of` widens the
  code to `u64` before shifting (at depth 0 the shift is 32, undefined on a `u32`); the ladder's
  ceiling is `1u64 << (2 · d)`; the rungs, the estimator's output and `OccupancyLadder::at` are all
  `u64`. A depth-16 tile index is the whole 32-bit Morton code and `N_occ(16)` can in principle be
  `2^32`, which does not fit a `u32` — and is not put in one.
- **The mask's ceiling and the row space's ceiling are the same number.** Roaring bitmaps are
  32-bit by construction, so a view of more than `u32::MAX` rows is not merely unguarded, it is not
  expressible. Rung 6 at 3.65 × 10⁹ is comfortably inside. **A rung above ~4.29 × 10⁹ rows in one
  view is not**, and reaching it is a bundle-format and mask-library change rather than a tuning
  one. That is worth knowing before a rung 7 is planned, and it is outside this change.

## Re-running against GBIF

```bash
# Exact N_occ(d), independent of the engine — no server needed.
reference/.venv/bin/python probes/2026-09-09-nocc-stage/n_occ_ladder.py \
  <bundle>/v00000/partitions/default/views/<view>/segments/seg-0/morton.u32

# First paint. Needs `serve.stage_timing = true` in the deployment's tessera.toml and a
# bench-timing build:
#   cargo build --release -p tessera-cli --features tessera-server/bench-timing
# `terms.json` is a JSON list of the access vocabulary's keys — full coverage.
reference/.venv/bin/python probes/2026-09-09-nocc-stage/first_paint.py \
  --viewer http://127.0.0.1:PORT --session http://127.0.0.1:PORT \
  --credential "$SESSION_CRED" --terms terms.json --view <view> \
  --reps 5 --settle 1 --json after.json
```

Add `--trajectory` for one session's zoom path — the landed fill's own case — instead of a fresh
session per depth.

For the *before* column, set `crate::stage::StageDeps::enabled` to `false` at construction and
rebuild; there is deliberately no configuration key for it (`Engine::set_occupancy_stage_for_test`
is the in-process hook and is `fault-injection`-gated). `Engine::occupancy_walks` counts every walk
a process has made and is the cheaper check where an embedder is available.

## What was not measured

* **Anything at 3.65 × 10⁹.** The section above is arithmetic and says so.
* **The sketch route at 2.33 × 10⁸.** `treeoflife` is single-segment, so every walk timed here is
  the counted route. The sketch's cost at that scale would need the corpus re-dealt into segments,
  which `occupancy_sketch` does by rebuilding the fixture in memory — 9 GB of `TilerItem` at this
  row count, which the box could not spare beside another session's build.
* **A multi-view or multi-session fill under load.** One session at a time.
* **The refresh path.** The rungs go stale on every publication and nothing warms them there; that
  is decision 0138's stated non-coverage, not a measurement gap.
