# Selection batched per shard

**Status:** measurement, 2026-10-10. Harness: `crates/mosaica-bench/src/bin/epoch_shard_treemap_mask.rs`
at fdf750a6, which adds the `select_batched` column and the `--abutting` option. Host: a cloud
container on an Intel Xeon at 2.80 GHz, 4 cores, with nothing else running; the bench is one thread.
Every figure is measured. This is the third of the measurements `docs/sharding.md` §8.1 leaves
before code.

## What was measured

The plan's §3.2 batches selection per shard as the count is: one cursor walk of each leaf over the
request's parts in row order, feeding the merge of the parts. `select_batched` is that walk. It
seeks only where a part does not begin where the last one ended, and feeds every visible row into
its tile's threshold count and bounded heap, which carry over from one leaf to the next. Tiles and
parts with no visible row are left out, as they are in `select_parts`, today's selection run once
per part with the empty parts left out. Each tile's rows equal `Selection::of`'s, checked on every
tile of every case.

The cases are those of `probes/2026-10-09-shard-read-costs/`'s Table 6: a synthetic universe of
2³⁰ rows at N = 1, 8, 32 and 100 leaves, coverage of 0.1%, 1%, 10% and 50%, contiguous requests of
3,000 and 256 tiles, depth 6 and deeper, three samples. They ran with two tile layouts:

- **abutting**: tiles next to each other in Morton order share one jittered boundary, as a
  request's tiles do in a segment;
- **jittered**: each part's start is jittered on its own by up to one container, as in the earlier
  probe, so neighbouring parts never meet and the walk seeks at every part.

[`run.sh`](run.sh) is the sequence. `medians.csv` holds every cell's median, which
`python3 extract.py` builds from the bench's JSON; `python3 tables.py abutting` and
`python3 tables.py jittered` print the tables below and the 256-tile and jittered ones.

## Findings

**Where tiles abut, batched selection takes a narrow viewer's request at depths 8 to 10 to 0.57 to
0.97 of selection per part, apart from 0.1% at depth 10.** At N = 100 a viewer of 1% at depth 8
pays 3.9 times today's single shard, against 6.1 with selection per part, and at depth 10 4.6
against 7.9. At 0.1% and depth 10 nothing changes. Viewers of 10% and 50% change little at depth 9
and shallower, and at depth 10 and N = 100 they fall from 6.2 and 6.3 times today's to 4.5 and 4.0.

**The gain is the seek between parts that meet.** With jittered tiles the same cells barely move:
1% at depth 8 and N = 100 is 6.3 times today's batched against 6.4 per part.

**At depth 11 and deeper the count sets what N costs, not selection.** With empty parts left out,
a narrow viewer's selection at N = 100 takes 0.3 to 2.2 ms, and `count_ranges` over the request's
300,000 parts takes 5.7 to 11.4 ms. A viewer of 0.1% then pays about 20 times today's at depth 11,
6 ms.

**Where a selection costs under a tenth of a millisecond, the walk costs more than selection per
part.** It keeps a slot for each of the request's tiles, about 0.05 ms at 3,000 tiles: at 0.1% and
depth 14, one shard, 0.062 ms against 0.006.

## Not measured

- The engine. These are synthetic leaves and a synthetic identity column of one row per cell.
- A filtered or highlighted request.
- More than one thread.
- A corpus whose viewers' items cluster in few shards, where leaving out the empty shards (§3.1)
  takes most of N's cost away.

## Tables

### Table 1: mask work per request, abutting tiles, 3,000 tiles

The count and the selection summed, ms. "Today" is the per-range count and `select_tiles` at
N = 1. "Per part" is `count_ranges` and `select_parts`; "batched" is `count_ranges` and
`select_batched`. The ratio in brackets is against today.

| coverage | depth | today N = 1 | N = 8 per part | N = 8 batched | N = 32 per part | N = 32 batched | N = 100 per part | N = 100 batched |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 50% | 6 | 2,285 | 2,312 (1.0×) | 2,412 (1.1×) | 2,519 (1.1×) | 2,465 (1.1×) | 2,726 (1.2×) | 2,570 (1.1×) |
| 50% | 7 | 581 | 633 (1.1×) | 688 (1.2×) | 655 (1.1×) | 691 (1.2×) | 924 (1.6×) | 862 (1.5×) |
| 50% | 8 | 173 | 187 (1.1×) | 192 (1.1×) | 245 (1.4×) | 230 (1.3×) | 443 (2.6×) | 455 (2.6×) |
| 50% | 9 | 71.3 | 68.0 (1.0×) | 68.2 (1.0×) | 137 (1.9×) | 127 (1.8×) | 289 (4.1×) | 273 (3.8×) |
| 50% | 10 | 30.1 | 43.3 (1.4×) | 42.8 (1.4×) | 80.4 (2.7×) | 78.0 (2.6×) | 191 (6.3×) | 122 (4.0×) |
| 50% | 11 | 19.9 | 26.5 (1.3×) | 24.3 (1.2×) | 54.5 (2.7×) | 40.1 (2.0×) | 91.8 (4.6×) | 73.2 (3.7×) |
| 50% | 12 | 13.8 | 20.9 (1.5×) | 16.7 (1.2×) | 32.3 (2.3×) | 32.9 (2.4×) | — | — |
| 50% | 13 | 9.88 | 8.91 (0.9×) | 5.84 (0.6×) | — | — | — | — |
| 50% | 14 | 10.8 | — | — | — | — | — | — |
| 10% | 6 | 1,159 | 1,114 (1.0×) | 1,182 (1.0×) | 1,276 (1.1×) | 1,221 (1.1×) | 1,549 (1.3×) | 1,320 (1.1×) |
| 10% | 7 | 276 | 290 (1.1×) | 307 (1.1×) | 384 (1.4×) | 377 (1.4×) | 504 (1.8×) | 625 (2.3×) |
| 10% | 8 | 91.1 | 108 (1.2×) | 96.9 (1.1×) | 156 (1.7×) | 145 (1.6×) | 294 (3.2×) | 284 (3.1×) |
| 10% | 9 | 37.4 | 48.6 (1.3×) | 50.4 (1.3×) | 78.8 (2.1×) | 72.8 (1.9×) | 180 (4.8×) | 157 (4.2×) |
| 10% | 10 | 21.8 | 26.3 (1.2×) | 31.7 (1.5×) | 53.8 (2.5×) | 44.2 (2.0×) | 136 (6.2×) | 98.0 (4.5×) |
| 10% | 11 | 13.2 | 14.8 (1.1×) | 10.6 (0.8×) | 29.5 (2.2×) | 22.1 (1.7×) | 32.7 (2.5×) | 30.0 (2.3×) |
| 10% | 12 | 10.2 | 6.43 (0.6×) | 3.77 (0.4×) | 11.7 (1.1×) | 11.8 (1.2×) | — | — |
| 10% | 13 | 8.57 | 1.38 (0.2×) | 1.35 (0.2×) | — | — | — | — |
| 10% | 14 | 10.2 | — | — | — | — | — | — |
| 1% | 6 | 253 | 215 (0.8×) | 241 (1.0×) | 314 (1.2×) | 333 (1.3×) | 396 (1.6×) | 379 (1.5×) |
| 1% | 7 | 63.7 | 74.3 (1.2×) | 78.3 (1.2×) | 110 (1.7×) | 131 (2.1×) | 218 (3.4×) | 174 (2.7×) |
| 1% | 8 | 25.2 | 40.0 (1.6×) | 38.7 (1.5×) | 84.9 (3.4×) | 55.7 (2.2×) | 154 (6.1×) | 98.8 (3.9×) |
| 1% | 9 | 10.5 | 22.9 (2.2×) | 19.0 (1.8×) | 45.2 (4.3×) | 39.4 (3.8×) | 64.2 (6.1×) | 55.4 (5.3×) |
| 1% | 10 | 5.33 | 12.9 (2.4×) | 7.40 (1.4×) | 25.3 (4.8×) | 18.3 (3.4×) | 42.0 (7.9×) | 24.7 (4.6×) |
| 1% | 11 | 1.12 | 3.05 (2.7×) | 2.40 (2.2×) | 7.41 (6.6×) | 8.05 (7.2×) | 13.6 (12.2×) | 12.9 (11.6×) |
| 1% | 12 | 0.516 | 1.14 (2.2×) | 1.14 (2.2×) | 4.13 (8.0×) | 4.23 (8.2×) | — | — |
| 1% | 13 | 0.338 | 0.856 (2.5×) | 0.866 (2.6×) | — | — | — | — |
| 1% | 14 | 0.271 | — | — | — | — | — | — |
| 0.1% | 6 | 38.1 | 47.0 (1.2×) | 47.5 (1.2×) | 97.1 (2.6×) | 74.0 (1.9×) | 159 (4.2×) | 133 (3.5×) |
| 0.1% | 7 | 18.0 | 30.9 (1.7×) | 25.7 (1.4×) | 59.4 (3.3×) | 43.5 (2.4×) | 88.6 (4.9×) | 95.3 (5.3×) |
| 0.1% | 8 | 5.65 | 11.4 (2.0×) | 6.87 (1.2×) | 23.0 (4.1×) | 15.7 (2.8×) | 33.1 (5.9×) | 26.5 (4.7×) |
| 0.1% | 9 | 1.72 | 3.11 (1.8×) | 2.23 (1.3×) | 6.70 (3.9×) | 4.22 (2.5×) | 12.0 (7.0×) | 10.9 (6.3×) |
| 0.1% | 10 | 0.579 | 1.12 (1.9×) | 1.23 (2.1×) | 2.46 (4.2×) | 2.37 (4.1×) | 6.27 (10.8×) | 6.14 (10.6×) |
| 0.1% | 11 | 0.303 | 0.556 (1.8×) | 0.644 (2.1×) | 1.97 (6.5×) | 1.98 (6.5×) | 6.02 (19.9×) | 6.04 (19.9×) |
| 0.1% | 12 | 0.209 | 0.420 (2.0×) | 0.453 (2.2×) | 1.71 (8.2×) | 1.76 (8.5×) | — | — |
| 0.1% | 13 | 0.185 | 0.339 (1.8×) | 0.371 (2.0×) | — | — | — | — |
| 0.1% | 14 | 0.178 | — | — | — | — | — | — |

### Table 2: selection batched over selection per part, abutting tiles, 3,000 tiles

| coverage | depth | N = 1 | N = 8 | N = 32 | N = 100 |
|---:|---:|---:|---:|---:|---:|
| 50% | 6 | 1.05 | 1.04 | 0.98 | 0.94 |
| 50% | 7 | 1.11 | 1.09 | 1.06 | 0.93 |
| 50% | 8 | 1.13 | 1.03 | 0.94 | 1.03 |
| 50% | 9 | 1.00 | 1.00 | 0.92 | 0.94 |
| 50% | 10 | 1.05 | 0.99 | 0.97 | 0.63 |
| 50% | 11 | 1.01 | 0.92 | 0.73 | 0.78 |
| 50% | 12 | 0.96 | 0.79 | 1.02 | — |
| 50% | 13 | 0.65 | 0.64 | — | — |
| 50% | 14 | 0.96 | — | — | — |
| 10% | 6 | 0.94 | 1.06 | 0.96 | 0.85 |
| 10% | 7 | 1.18 | 1.06 | 0.98 | 1.25 |
| 10% | 8 | 1.10 | 0.89 | 0.93 | 0.96 |
| 10% | 9 | 0.95 | 1.04 | 0.92 | 0.86 |
| 10% | 10 | 1.01 | 1.21 | 0.82 | 0.71 |
| 10% | 11 | 1.04 | 0.71 | 0.74 | 0.90 |
| 10% | 12 | 0.72 | 0.55 | 1.01 | — |
| 10% | 13 | 0.94 | 0.97 | — | — |
| 10% | 14 | 1.27 | — | — | — |
| 1% | 6 | 1.09 | 1.12 | 1.06 | 0.96 |
| 1% | 7 | 1.20 | 1.06 | 1.20 | 0.79 |
| 1% | 8 | 0.78 | 0.97 | 0.63 | 0.60 |
| 1% | 9 | 1.01 | 0.82 | 0.86 | 0.83 |
| 1% | 10 | 0.38 | 0.54 | 0.67 | 0.46 |
| 1% | 11 | 0.70 | 0.70 | 1.17 | 0.70 |
| 1% | 12 | 0.97 | 1.01 | 1.20 | — |
| 1% | 13 | 1.31 | 1.09 | — | — |
| 1% | 14 | 1.64 | — | — | — |
| 0.1% | 6 | 1.14 | 1.01 | 0.75 | 0.83 |
| 0.1% | 7 | 1.48 | 0.83 | 0.72 | 1.08 |
| 0.1% | 8 | 0.84 | 0.58 | 0.65 | 0.76 |
| 0.1% | 9 | 0.72 | 0.66 | 0.48 | 0.82 |
| 0.1% | 10 | 1.03 | 1.15 | 0.90 | 0.89 |
| 0.1% | 11 | 1.32 | 1.64 | 1.03 | 1.04 |
| 0.1% | 12 | 2.00 | 1.74 | 1.57 | — |
| 0.1% | 13 | 3.81 | 2.63 | — | — |
| 0.1% | 14 | 11.12 | — | — | — |
