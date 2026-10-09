# What shards cost the read path, measured again with a batched count

**Status:** measurement, 2026-10-09, at commit 790efb3a. Synthetic and in memory, no bundle. Host:
a cloud container on an Intel Xeon at 2.80 GHz, 4 cores, 15 GB, kernel 6.18, with nothing else
running. Every figure is single-threaded and measured unless marked modelled. The 2026-09-04
probes ran on a 12-core Ryzen 9 5900X with 47 GB, so their absolute times and these differ; the
ratios between shard counts are what compare.

Harnesses: `crates/mosaica-bench/src/bin/epoch_shard_treemap_mask.rs`, extended at 790efb3a, and
`crates/mosaica-bench/src/bin/epoch_shard_projection.rs`, unchanged. Sequence: [`run.sh`](run.sh).
Every cell with its samples: `treemap-*.json`; the bench's own tables: `treemap-*.md`; the
projection's raw output: `runs/`, folded by `collate_projection.py` into `projection-result.json`.

This is §8.1 of [the sharding plan](../../docs/sharding.md). The tile-index bench,
`epoch_shard_tile_index`, did not run: it reads the MedCPT and PaperSeek bundles, which are not
on this host.

## Findings

**A batched count takes most of the shard cost out of counting, and is cheaper than today's count
at one shard.** `count_ranges` is one `roaring_bitmap_rank_many` call per shard over the request's
sorted tile endpoints. On a contiguous 3,000-tile request at depth 10 to 12 and 10% coverage it
takes 0.15 to 0.38 ms at N = 1, where today's per-range count takes 7.7 to 8.0 ms. At depth 10
and 11 and N = 100 it takes 6.2 to 8.5 ms, where today's count takes 875 to 887 ms. On a 256-tile viewport at depth 8 to 12 and
coverage of 10% or more it takes 0.06 to 0.26 ms at N = 1 against 0.6 to 0.7 ms.

**It helps only where a request's ranges share containers.** On 256 tiles scattered at random
over the map, `count_ranges` at N = 100 is still 29 to 69 times its own N = 1 cost at 10%
coverage and depth 4 or deeper, against 100 to 160 times for today's count. At 1% coverage and below, where containers
are arrays and today's count is two binary searches, batching saves nothing at N = 1. It also adds
a fixed 40 to 80 µs on a scattered request, because `rank_many` reads every container header of
the leaf.

**Once the count is batched, decoding and selection set the cost of N.** Both pay a fixed cost per
part, a cursor seek or a part of the selection's merge, so their cost grows with N times tiles
where a tile holds few visible rows. Table 1 adds the three operations a viewport request makes
on the mask. Against today's single shard, a request at N = 8 costs 0.9 to 1.7 times as much at
10% coverage and above. A narrow viewer at depth 8 and deeper pays more: 2.0 to 8.4 times at N = 8
for 0.1% and 1%, which is 0.8 to 59 ms in absolute terms. At N = 100 dense viewers pay up to 12
times and narrow ones up to 83 times. At depth 6 and coverage of 10% or more the work per row
dominates, and N costs 1.0 to 1.4 times up to N = 100.

**The range-local count is the cheapest way to count a short range.** `count_local` counts
`leaf ∩ range` against a one-run bitmap, which popcounts only the words inside the range. Today's
count takes two ranks, each a popcount from the start of the range's container. On 256 random
tiles at depth 10 and 12 and coverage of 10% or more, `count_local` takes 0.03 to 0.05 ms at N = 1
against 0.7 ms for today's count. On long ranges it is the slowest: 1.2 to 2.3 ms at depth 0
against 0.2 to 0.4 ms. Its cost per part, 0.15 to 0.3 µs, still grows with N.

**`rows_in_range`, the materialised `leaf ∩ range` a decode reads while the overlay holds
deletions or suppressions, grows worst with N at shallow depths.** On the 3,000-tile viewport at
depth 6 and 50% coverage it goes from 18 ms at N = 1 to 965 ms at N = 100.

**A level's figures cost the same to walk per shard, and the sum over shards matters only for
narrow viewers.** The walk reads each visible row's label from a column and counts it against one
of 1,000,000 artifacts. At 50% coverage the walk and the sum take 3.79 s at N = 1 and 3.98 s at
N = 100 (1.05 times). Summing the shards' count vectors costs about 0.8 ms per shard at a million
artifacts. That is noise against a dense viewer's walk and dominant for a narrow one: at 0.1%
coverage N = 100 costs 3.4 times N = 1, 68 ms of sum against a 78 ms walk.

**Projecting a session's grant per shard costs what one projection costs.** One mask over 10⁸
entities projected through N permutations of 10⁸/N, against one permutation of 10⁸, took 0.76 to
1.28 times as long at N = 8, 32 and 100 and coverage of 1% to 25% (Table 4). Eight leaves per token
held 10⁴ at once took 1.09 times the resident memory and 1.30 times the build time of one.

**Projection is not linear in rows on this host past 10⁸.** At 25% coverage the cost per row
rises from 2.7 ns at 10⁸ to 5.2 ns at 4×10⁸, and at 10% from 1.5 ns to 2.2 ns. At 1% it stays at
0.42 ns. The 2026-09-04 probe found the cost per row flat between 10⁸ and 4×10⁸ on the 12-core
host (0.94 to 1.05 times per decade). The 4×10⁸ permutation is 1.6 GB read at random, so the
difference is the hosts' caches and memory. Whether projection is linear at a shard of 2³²
entities therefore depends on the deployment's hardware, and is measured there.

## Table 1: mask work per viewport request

Count, decode and select summed, in ms per request, contiguous tiles. "Today" is the per-range
count at N = 1. Every other column uses `count_ranges`. Ratios are against today.

| tiles | coverage | depth | today N = 1 | batched N = 1 | N = 8 | N = 32 | N = 100 | N = 8 ÷ today | N = 32 ÷ today | N = 100 ÷ today |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 256 | 50% | 6 | 309 | 309 | 308 | 318 | 346 | 1.00 | 1.03 | 1.12 |
| 256 | 50% | 8 | 21.9 | 21.5 | 22.3 | 30.9 | 52.5 | 1.02 | 1.41 | 2.40 |
| 256 | 50% | 10 | 2.80 | 2.28 | 4.34 | 9.05 | 23.2 | 1.55 | 3.23 | 8.29 |
| 256 | 50% | 12 | 1.04 | 0.43 | 1.08 | 4.23 | — | 1.04 | 4.09 | — |
| 256 | 10% | 6 | 117 | 117 | 120 | 153 | 161 | 1.03 | 1.30 | 1.37 |
| 256 | 10% | 8 | 8.98 | 8.57 | 11.0 | 17.4 | 34.3 | 1.22 | 1.93 | 3.81 |
| 256 | 10% | 10 | 1.55 | 1.03 | 2.24 | 8.32 | 18.7 | 1.44 | 5.37 | 12.09 |
| 256 | 10% | 12 | 0.84 | 0.24 | 0.76 | 4.12 | — | 0.90 | 4.90 | — |
| 256 | 1% | 6 | 19.7 | 19.7 | 23.8 | 27.0 | 44.9 | 1.21 | 1.37 | 2.28 |
| 256 | 1% | 8 | 1.71 | 1.70 | 4.15 | 8.30 | 20.8 | 2.42 | 4.85 | 12.19 |
| 256 | 1% | 10 | 0.25 | 0.25 | 1.08 | 3.66 | 11.9 | 4.24 | 14.38 | 46.85 |
| 256 | 0.1% | 6 | 6.35 | 6.35 | 6.87 | 9.66 | 25.1 | 1.08 | 1.52 | 3.96 |
| 256 | 0.1% | 8 | 0.40 | 0.39 | 1.40 | 2.85 | 9.20 | 3.54 | 7.21 | 23.28 |
| 256 | 0.1% | 10 | 0.09 | 0.09 | 0.76 | 2.30 | 7.56 | 8.39 | 25.42 | 83.45 |
| 3,000 | 50% | 6 | 3,553 | 3,553 | 3,668 | 3,888 | 4,058 | 1.03 | 1.09 | 1.14 |
| 3,000 | 50% | 8 | 253 | 248 | 263 | 348 | 649 | 1.04 | 1.37 | 2.56 |
| 3,000 | 50% | 10 | 36.9 | 29.3 | 57.8 | 107 | 293 | 1.57 | 2.91 | 7.94 |
| 3,000 | 50% | 12 | 14.0 | 6.44 | 24.1 | 51.4 | — | 1.71 | 3.66 | — |
| 3,000 | 10% | 6 | 1,456 | 1,456 | 1,507 | 1,587 | 1,854 | 1.03 | 1.09 | 1.27 |
| 3,000 | 10% | 8 | 112 | 106 | 119 | 214 | 395 | 1.06 | 1.91 | 3.53 |
| 3,000 | 10% | 10 | 22.2 | 14.6 | 36.2 | 84.4 | 228 | 1.63 | 3.80 | 10.28 |
| 3,000 | 10% | 12 | 9.87 | 2.31 | 12.0 | 33.9 | — | 1.21 | 3.43 | — |
| 3,000 | 1% | 6 | 257 | 258 | 319 | 334 | 512 | 1.24 | 1.30 | 1.99 |
| 3,000 | 1% | 8 | 29.2 | 29.1 | 59.2 | 98.7 | 249 | 2.02 | 3.37 | 8.52 |
| 3,000 | 1% | 10 | 7.08 | 6.96 | 21.3 | 51.7 | 142 | 3.01 | 7.30 | 20.03 |
| 3,000 | 0.1% | 6 | 46.2 | 46.1 | 69.4 | 117 | 257 | 1.50 | 2.52 | 5.56 |
| 3,000 | 0.1% | 8 | 6.41 | 6.25 | 18.3 | 48.5 | 127 | 2.86 | 7.57 | 19.81 |
| 3,000 | 0.1% | 10 | 1.14 | 0.97 | 6.72 | 28.5 | 86.6 | 5.89 | 24.97 | 75.85 |

A dash is a depth at which a tile holds fewer rows than shards, which the bench skips.

## Table 2: three ways to count, at N = 1

ms per request, median of samples.

| layout | coverage | depth | today's count | `count_ranges` | `count_local` |
|---|---:|---:|---:|---:|---:|
| 256 random | 50% | 8 | 0.72 | 0.67 | 0.20 |
| 256 random | 50% | 12 | 0.70 | 0.51 | 0.04 |
| 256 random | 10% | 8 | 0.73 | 0.72 | 0.18 |
| 256 random | 10% | 12 | 0.70 | 0.57 | 0.03 |
| 256 random | 1% | 12 | 0.03 | 0.07 | 0.05 |
| 256 viewport | 50% | 8 | 0.65 | 0.26 | 0.18 |
| 256 viewport | 50% | 12 | 0.66 | 0.06 | 0.02 |
| 256 viewport | 10% | 8 | 0.65 | 0.23 | 0.17 |
| 256 viewport | 10% | 12 | 0.67 | 0.06 | 0.02 |
| 256 viewport | 1% | 12 | 0.02 | 0.02 | 0.03 |
| 3,000 viewport | 50% | 6 | 11.7 | 12.0 | 20.3 |
| 3,000 viewport | 50% | 10 | 8.00 | 0.37 | 0.41 |
| 3,000 viewport | 10% | 6 | 11.5 | 11.4 | 16.7 |
| 3,000 viewport | 10% | 10 | 8.01 | 0.38 | 0.40 |
| 3,000 viewport | 1% | 10 | 0.36 | 0.23 | 0.83 |
| 3,000 viewport | 0.1% | 10 | 0.23 | 0.06 | 0.39 |

## Table 3: one operation at N = 100 over N = 1, 3,000-tile viewport

| coverage | depth | today's count | `count_ranges` | `count_local` | decode | `rows_in_range` | select |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 50% | 6 | 76.7 | 4.0 | 6.4 | 1.1 | 53.9 | 1.2 |
| 50% | 8 | 111.2 | 5.8 | 22.3 | 1.9 | 26.9 | 2.9 |
| 50% | 10 | 103.9 | 23.2 | 116.2 | 12.7 | 23.5 | 9.3 |
| 10% | 6 | 80.1 | 4.8 | 7.4 | 1.3 | 26.5 | 1.2 |
| 10% | 8 | 103.3 | 7.1 | 24.0 | 4.6 | 9.8 | 3.5 |
| 10% | 10 | 109.2 | 22.2 | 108.1 | 43.2 | 22.9 | 12.7 |
| 1% | 8 | 110.2 | 66.6 | 46.7 | 65.7 | 70.5 | 6.3 |
| 1% | 10 | 104.9 | 76.4 | 63.9 | 78.9 | 76.3 | 12.6 |
| 0.1% | 8 | 122.6 | 62.3 | 79.0 | 103.8 | 88.9 | 11.9 |
| 0.1% | 10 | 133.4 | 91.0 | 114.3 | 111.7 | 104.9 | 74.2 |

## Table 4: one mask over 10⁸ entities, N permutations against one

Median of five, ms. "With split" adds the emulation's cost of restricting the mask to each shard,
which a shard-aware permutation would not pay. `project` is the session's path; `project_with`
reuses one scratch.

| N | entry point | coverage | one | N leaves | N leaves ÷ one | with split |
|---:|---|---:|---:|---:|---:|---:|
| 8 | `project` | 25% | 265.2 | 252.4 | 0.95 | 1.00 |
| 8 | `project` | 10% | 147.8 | 139.4 | 0.94 | 1.00 |
| 8 | `project` | 1% | 39.7 | 46.5 | 1.17 | 1.23 |
| 32 | `project` | 25% | 292.3 | 230.1 | 0.79 | 0.82 |
| 32 | `project` | 10% | 141.6 | 134.3 | 0.95 | 1.00 |
| 32 | `project` | 1% | 41.8 | 37.5 | 0.90 | 0.95 |
| 100 | `project` | 25% | 289.2 | 221.1 | 0.76 | 0.80 |
| 100 | `project` | 10% | 157.8 | 126.4 | 0.80 | 0.85 |
| 100 | `project` | 1% | 41.2 | 52.8 | 1.28 | 1.34 |
| 8 | `project_with` | 25% | 244.4 | 223.7 | 0.92 | 0.95 |
| 32 | `project_with` | 25% | 240.6 | 245.4 | 1.02 | 1.05 |
| 100 | `project_with` | 25% | 233.5 | 204.5 | 0.88 | 0.91 |

The linearity table and the token table are printed by `python3 collate_projection.py`. Its
heading over the shard table names N = 100 for every row; the rows are N = 100, 32 and 8 in that
order within each coverage, as their container counts (1,600, 1,536, 1,528) show.

## Table 5: a level's figures walk, 1,000,000 artifacts, 256-tile random run

ms per walk, median of five.

| coverage | N | walk | sum of the shards' counts | both ÷ N = 1 |
|---:|---:|---:|---:|---:|
| 50% | 1 | 3,788 | 0.82 | 1.00 |
| 50% | 8 | 3,829 | 6.95 | 1.01 |
| 50% | 100 | 3,905 | 77.4 | 1.05 |
| 10% | 1 | 1,081 | 0.82 | 1.00 |
| 10% | 100 | 1,237 | 84.6 | 1.22 |
| 1% | 1 | 216 | 0.75 | 1.00 |
| 1% | 100 | 269 | 81.3 | 1.62 |
| 0.1% | 1 | 42.3 | 1.04 | 1.00 |
| 0.1% | 32 | 63.4 | 24.6 | 2.03 |
| 0.1% | 100 | 78.4 | 67.9 | 3.38 |

## How the cases are built

The mask bench's model is the 2026-09-04 probe's: a universe of 2³⁰ rows, one bitmap at N = 1 or N
leaves of 2³⁰/N rows at the same density, and a tile at depth d one range of 2³⁰/4ᵈ rows, or N
ranges of 2³⁰/(N·4ᵈ), one per leaf at the same map position. Each part starts up to one container
past its nominal start, as a real tile's range does. Rows are drawn independently, except in the
run-heavy variant, whose tables are in `treemap-*.md`.

- `--layout random` places a request's tiles anywhere, as the 2026-09-04 probe did. `--layout
  viewport` places them as a contiguous block of the map in Morton order, as a screen asks for
  them.
- `count_ranges` sorts each leaf's endpoints outside the timer. In the engine a request's tiles at
  one depth are disjoint and leave `tile_ranges_all` in row order, so they arrive sorted; only this
  bench's per-part offsets can reorder them.
- `count_local` builds its one-run bitmap inside the timer, as an engine call would.
- Every part, including one with no visible row, goes to decode and select, as the bench did on
  2026-09-04.
- The figures walk's labels are drawn uniformly at random, so its count vector is written at random
  addresses. A real level's labels follow the map, which would make the walk cheaper and leave the
  sum unchanged.
- Each of the 256-tile random cells is the median of five samples after one warm-up; each viewport
  cell of three.
