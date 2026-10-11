# What shards cost the read path, measured again with a batched count

**Status:** measurement, 2026-10-09, at commit 790efb3a. Synthetic and in memory, no bundle. Host:
a cloud container on an Intel Xeon at 2.80 GHz, 4 cores, 15 GB, kernel 6.18, with nothing else
running. Every figure is single-threaded and measured unless marked modelled. The 2026-09-04
probes ran on a 12-core Ryzen 9 5900X with 47 GB, so their absolute times and these differ; the
ratios between shard counts are what compare.

Harnesses: `crates/mosaica-bench/src/bin/epoch_shard_treemap_mask.rs`, extended at 790efb3a, and
`crates/mosaica-bench/src/bin/epoch_shard_projection.rs`, unchanged. Sequence: [`run.sh`](run.sh).
`run.sh` writes each run's cells as JSON and tables beside it, and the projection's raw output into
`runs/`, which `collate_projection.py` folds into `projection-result.json`. Those outputs are not
kept in the tree; the tables below are taken from them.

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

**Once the count is batched, selection sets the cost of N.** Selection reads each part's visible
rows and merges the parts' identities, and pays a fixed cost per part, a cursor seek and the part's
share of the merge, so its cost grows with N times tiles where a tile holds few visible rows.
Table 1 adds the count and the selection, the two operations a viewport request makes on the mask.
Against today's single shard, a request at N = 8 costs 0.7 to 1.6 times as much at 10% coverage
and above. A narrow viewer at depth 8 and deeper pays more: 1.9 to 8.1 times at N = 8 for 0.1% and
1%, which is 0.3 to 55 ms in absolute terms. At N = 100 dense viewers pay up to 10 times and narrow
ones up to 64 times. At depth 6 and coverage of 10% or more the work per row dominates, and N costs
1.0 to 1.4 times up to N = 100.

**Skipping the parts of a tile with no visible row takes most of N's cost off a narrow viewer.**
Today's sweep skips a tile with nothing visible but passes every part of the rest to selection,
including parts with no visible row, and each costs a seek and a place in the merge. A second run,
`run.sh parts` on 2026-10-10, measured selection both ways, `select_tiles` and `select_parts`, at
depth 6 and deeper on the contiguous layouts, with the same rows served on every tile (Table 6).
With the count batched as well, a viewer of 0.1% or 1% at depth 8 and deeper pays 1.2 to 3.1 times
today's single shard at N = 8 and 5 to 12 times at N = 100, against 1.5 to 5.2 and up to 57 times
with the count batched alone. A viewer of 10% or more changes little, because few of its parts are
empty: 0.6 to 1.7 times at N = 8 and up to 11 times at N = 100. What remains is the cost per part of
parts that hold rows.

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

Count and select summed, in ms per request, contiguous tiles. Select reads each part's visible rows
itself, which the decode column measures on its own, so decode is not added again. "Today" is the
per-range count at N = 1. Every other column uses `count_ranges`. Ratios are against today.

| tiles | coverage | depth | today N = 1 | batched N = 1 | N = 8 | N = 32 | N = 100 | N = 8 ÷ today | N = 32 ÷ today | N = 100 ÷ today |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 256 | 50% | 6 | 208 | 208 | 203 | 212 | 237 | 0.98 | 1.02 | 1.14 |
| 256 | 50% | 8 | 15.2 | 14.8 | 15.6 | 23.5 | 41.2 | 1.03 | 1.55 | 2.71 |
| 256 | 50% | 10 | 2.40 | 1.87 | 3.72 | 7.61 | 19.0 | 1.55 | 3.17 | 7.93 |
| 256 | 50% | 12 | 0.99 | 0.38 | 0.82 | 3.46 | — | 0.83 | 3.51 | — |
| 256 | 10% | 6 | 95.0 | 94.9 | 97.9 | 129 | 128 | 1.03 | 1.35 | 1.35 |
| 256 | 10% | 8 | 7.64 | 7.22 | 9.29 | 14.7 | 27.7 | 1.22 | 1.93 | 3.63 |
| 256 | 10% | 10 | 1.44 | 0.92 | 1.87 | 7.11 | 14.8 | 1.30 | 4.94 | 10.31 |
| 256 | 10% | 12 | 0.79 | 0.19 | 0.53 | 3.02 | — | 0.67 | 3.83 | — |
| 256 | 1% | 6 | 19.1 | 19.1 | 22.9 | 24.9 | 38.5 | 1.20 | 1.30 | 2.01 |
| 256 | 1% | 8 | 1.64 | 1.63 | 3.76 | 6.94 | 16.3 | 2.28 | 4.22 | 9.93 |
| 256 | 1% | 10 | 0.22 | 0.22 | 0.78 | 2.33 | 7.80 | 3.57 | 10.69 | 35.75 |
| 256 | 1% | 12 | 0.06 | 0.06 | 0.40 | 1.60 | — | 6.26 | 25.07 | — |
| 256 | 0.1% | 6 | 6.21 | 6.21 | 6.01 | 8.32 | 19.7 | 0.97 | 1.34 | 3.17 |
| 256 | 0.1% | 8 | 0.32 | 0.31 | 0.97 | 1.72 | 5.52 | 3.05 | 5.45 | 17.44 |
| 256 | 0.1% | 10 | 0.06 | 0.06 | 0.49 | 1.19 | 3.86 | 8.14 | 19.62 | 63.72 |
| 256 | 0.1% | 12 | 0.05 | 0.05 | 0.29 | 1.16 | — | 5.40 | 21.48 | — |
| 3,000 | 50% | 6 | 2,325 | 2,325 | 2,439 | 2,544 | 2,729 | 1.05 | 1.09 | 1.17 |
| 3,000 | 50% | 8 | 179 | 174 | 185 | 259 | 510 | 1.03 | 1.44 | 2.84 |
| 3,000 | 50% | 10 | 32.1 | 24.5 | 49.9 | 87.7 | 232 | 1.56 | 2.73 | 7.23 |
| 3,000 | 50% | 12 | 13.4 | 5.85 | 20.6 | 41.9 | — | 1.53 | 3.11 | — |
| 3,000 | 10% | 6 | 1,173 | 1,173 | 1,224 | 1,297 | 1,494 | 1.04 | 1.11 | 1.27 |
| 3,000 | 10% | 8 | 95.3 | 89.3 | 98.7 | 179 | 318 | 1.03 | 1.88 | 3.34 |
| 3,000 | 10% | 10 | 20.9 | 13.3 | 32.0 | 69.0 | 171 | 1.53 | 3.30 | 8.20 |
| 3,000 | 10% | 12 | 9.46 | 1.90 | 9.30 | 22.6 | — | 0.98 | 2.39 | — |
| 3,000 | 1% | 6 | 249 | 250 | 300 | 307 | 446 | 1.21 | 1.23 | 1.79 |
| 3,000 | 1% | 8 | 28.4 | 28.2 | 54.9 | 81.6 | 192 | 1.94 | 2.88 | 6.76 |
| 3,000 | 1% | 10 | 6.49 | 6.36 | 17.6 | 35.7 | 95.1 | 2.71 | 5.50 | 14.66 |
| 3,000 | 1% | 12 | 1.16 | 1.00 | 4.66 | 22.2 | — | 4.03 | 19.16 | — |
| 3,000 | 0.1% | 6 | 44.8 | 44.7 | 64.7 | 101 | 210 | 1.44 | 2.24 | 4.67 |
| 3,000 | 0.1% | 8 | 5.89 | 5.73 | 14.9 | 34.9 | 73.5 | 2.53 | 5.92 | 12.47 |
| 3,000 | 0.1% | 10 | 0.78 | 0.61 | 3.92 | 16.1 | 46.7 | 5.00 | 20.52 | 59.51 |
| 3,000 | 0.1% | 12 | 0.61 | 0.45 | 3.32 | 14.2 | — | 5.42 | 23.24 | — |

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

After a run, `python3 collate_projection.py` prints the linearity table and the token table. Its
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

## Table 6: mask work with empty parts skipped

Count and select summed, in ms per request, contiguous tiles, from `run.sh parts`.
"Today" is the per-range count and `select_tiles` at N = 1. "Count batched" is `count_ranges` and
`select_tiles`; "and skipped" is `count_ranges` and `select_parts`. Each cell is the median of
three samples; the ratio in brackets is against today.

| tiles | coverage | depth | today N = 1 | N = 8, count batched | N = 8, and empty parts skipped | N = 32, count batched | N = 32, and skipped | N = 100, count batched | N = 100, and skipped |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 256 | 50% | 6 | 194 | 195 (1.0×) | 200 (1.0×) | 222 (1.1×) | 208 (1.1×) | 234 (1.2×) | 231 (1.2×) |
| 256 | 50% | 8 | 14.6 | 15.6 (1.1×) | 15.8 (1.1×) | 22.7 (1.6×) | 22.8 (1.6×) | 38.4 (2.6×) | 38.0 (2.6×) |
| 256 | 50% | 10 | 2.40 | 2.51 (1.0×) | 2.49 (1.0×) | 7.45 (3.1×) | 7.46 (3.1×) | 19.4 (8.1×) | 18.6 (7.8×) |
| 256 | 50% | 12 | 1.12 | 0.80 (0.7×) | 0.79 (0.7×) | 2.07 (1.8×) | 1.83 (1.6×) | — | — |
| 256 | 10% | 6 | 104 | 108 (1.0×) | 112 (1.1×) | 118 (1.1×) | 113 (1.1×) | 142 (1.4×) | 136 (1.3×) |
| 256 | 10% | 8 | 8.35 | 9.89 (1.2×) | 8.82 (1.1×) | 14.8 (1.8×) | 15.7 (1.9×) | 25.1 (3.0×) | 25.3 (3.0×) |
| 256 | 10% | 10 | 1.39 | 1.61 (1.2×) | 1.58 (1.1×) | 6.62 (4.7×) | 6.57 (4.7×) | 17.1 (12.3×) | 15.6 (11.2×) |
| 256 | 10% | 12 | 0.78 | 0.62 (0.8×) | 0.50 (0.6×) | 1.77 (2.3×) | 0.91 (1.2×) | — | — |
| 256 | 1% | 6 | 23.1 | 20.5 (0.9×) | 20.2 (0.9×) | 33.7 (1.5×) | 33.0 (1.4×) | 39.1 (1.7×) | 41.9 (1.8×) |
| 256 | 1% | 8 | 1.62 | 2.48 (1.5×) | 1.88 (1.2×) | 7.37 (4.5×) | 7.27 (4.5×) | 15.5 (9.5×) | 18.2 (11.2×) |
| 256 | 1% | 10 | 0.20 | 0.70 (3.5×) | 0.62 (3.1×) | 3.59 (17.9×) | 2.07 (10.3×) | 6.80 (33.9×) | 2.11 (10.5×) |
| 256 | 1% | 12 | 0.05 | 0.23 (4.6×) | 0.11 (2.3×) | 0.89 (18.1×) | 0.39 (7.9×) | — | — |
| 256 | 0.1% | 6 | 4.20 | 3.63 (0.9×) | 2.88 (0.7×) | 8.77 (2.1×) | 8.14 (1.9×) | 18.9 (4.5×) | 17.2 (4.1×) |
| 256 | 0.1% | 8 | 0.20 | 0.58 (2.8×) | 0.52 (2.5×) | 1.84 (9.0×) | 1.05 (5.1×) | 5.15 (25.1×) | 1.67 (8.1×) |
| 256 | 0.1% | 10 | 0.05 | 0.22 (4.5×) | 0.09 (1.8×) | 0.91 (18.7×) | 0.22 (4.6×) | 2.75 (56.6×) | 0.56 (11.6×) |
| 256 | 0.1% | 12 | 0.02 | 0.06 (2.5×) | 0.05 (2.0×) | 0.22 (9.0×) | 0.15 (6.3×) | — | — |
| 3,000 | 50% | 6 | 2,330 | 2,344 (1.0×) | 2,391 (1.0×) | 2,480 (1.1×) | 2,557 (1.1×) | 2,862 (1.2×) | 2,817 (1.2×) |
| 3,000 | 50% | 8 | 176 | 188 (1.1×) | 189 (1.1×) | 266 (1.5×) | 267 (1.5×) | 469 (2.7×) | 443 (2.5×) |
| 3,000 | 50% | 10 | 32.7 | 45.3 (1.4×) | 47.9 (1.5×) | 99.4 (3.0×) | 86.5 (2.6×) | 252 (7.7×) | 224 (6.8×) |
| 3,000 | 50% | 12 | 12.0 | 17.4 (1.5×) | 17.8 (1.5×) | 39.9 (3.3×) | 36.6 (3.0×) | — | — |
| 3,000 | 10% | 6 | 1,222 | 1,237 (1.0×) | 1,262 (1.0×) | 1,322 (1.1×) | 1,384 (1.1×) | 1,597 (1.3×) | 1,700 (1.4×) |
| 3,000 | 10% | 8 | 96.7 | 103 (1.1×) | 103 (1.1×) | 166 (1.7×) | 203 (2.1×) | 327 (3.4×) | 321 (3.3×) |
| 3,000 | 10% | 10 | 22.6 | 30.9 (1.4×) | 38.6 (1.7×) | 68.2 (3.0×) | 67.2 (3.0×) | 160 (7.1×) | 150 (6.6×) |
| 3,000 | 10% | 12 | 14.2 | 9.90 (0.7×) | 8.34 (0.6×) | 23.2 (1.6×) | 12.0 (0.8×) | — | — |
| 3,000 | 1% | 6 | 331 | 282 (0.9×) | 327 (1.0×) | 309 (0.9×) | 319 (1.0×) | 424 (1.3×) | 453 (1.4×) |
| 3,000 | 1% | 8 | 27.2 | 40.8 (1.5×) | 39.9 (1.5×) | 86.6 (3.2×) | 79.5 (2.9×) | 182 (6.7×) | 164 (6.0×) |
| 3,000 | 1% | 10 | 5.83 | 13.6 (2.3×) | 12.4 (2.1×) | 42.8 (7.3×) | 26.6 (4.6×) | 76.1 (13.0×) | 29.5 (5.1×) |
| 3,000 | 1% | 12 | 0.57 | 2.97 (5.2×) | 1.45 (2.5×) | 16.0 (27.9×) | 6.45 (11.2×) | — | — |
| 3,000 | 0.1% | 6 | 69.9 | 76.5 (1.1×) | 68.7 (1.0×) | 89.1 (1.3×) | 94.3 (1.3×) | 184 (2.6×) | 178 (2.5×) |
| 3,000 | 0.1% | 8 | 5.88 | 13.9 (2.4×) | 13.1 (2.2×) | 31.7 (5.4×) | 23.2 (4.0×) | 66.7 (11.3×) | 38.9 (6.6×) |
| 3,000 | 0.1% | 10 | 0.66 | 2.91 (4.4×) | 1.13 (1.7×) | 12.9 (19.5×) | 3.76 (5.7×) | 33.1 (50.3×) | 7.00 (10.6×) |
| 3,000 | 0.1% | 12 | 0.26 | 0.66 (2.5×) | 0.46 (1.7×) | 2.37 (9.0×) | 1.62 (6.1×) | — | — |

## How the cases are built

The mask bench's model is the 2026-09-04 probe's: a universe of 2³⁰ rows, one bitmap at N = 1 or N
leaves of 2³⁰/N rows at the same density, and a tile at depth d one range of 2³⁰/4ᵈ rows, or N
ranges of 2³⁰/(N·4ᵈ), one per leaf at the same map position. Each part starts up to one container
past its nominal start, as a real tile's range does. Rows are drawn independently, except in a
run-heavy variant at 10%, which `run.sh treemap` also measures.

- `--layout random` places a request's tiles anywhere, as the 2026-09-04 probe did. `--layout
  viewport` places them as a contiguous block of the map in Morton order, as a screen asks for
  them.
- `count_ranges` sorts each leaf's endpoints outside the timer. In the engine a request's tiles at
  one depth are disjoint and leave `tile_ranges_all` in row order, so they arrive sorted; only this
  bench's per-part offsets can reorder them.
- `count_local` builds its one-run bitmap inside the timer, as an engine call would.
- Every part, including one with no visible row, goes to decode and select, as the bench did on
  2026-09-04. `select_tiles` skips a tile with no visible row, as `tile_sweep` does, and
  `select_parts` also each part with none; both are checked to serve the same rows on every tile.
- The figures walk's labels are drawn uniformly at random, so its count vector is written at random
  addresses. A real level's labels follow the map, which would make the walk cheaper and leave the
  sum unchanged.
- Each of the 256-tile random cells is the median of five samples after one warm-up; each viewport
  cell of three.
