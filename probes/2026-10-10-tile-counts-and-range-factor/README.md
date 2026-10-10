# A request's tiles counted in one walk, and the aggregate's choice between ranges and the pass

**Status:** measurement, 2026-10-10. Before: the engine at ee97910d. After: 41d6de9a, where
4a627c30 counts a request's tiles in one walk and 41d6de9a corrects `count_ranges` for a range
from row 0. Host: a cloud container on an Intel Xeon at 2.80 GHz, 4 cores, 15 GB, with nothing
else running. Every figure is measured.

## What changed

The viewport sweep counted each part of each tile, a tile's rows in one segment, with its own
range counts. An unfiltered request without a highlight counted each part three times: the visible
count, the matched count and the highlighted count, which equal each other there. Each count took
two ranks of each of the mask's bitmaps, and a rank in a bitset container is a popcount from the
container's start, so tiles sharing a container popcounted it again for each.

After 4a627c30 the sweep counts every part of every tile with one `EffectiveMask::count_ranges`
before the tiles go to the pool, and the matched and highlighted counts take the count below them,
which they equal without a filter or a highlight.

The aggregate counts a group's cells by range where the group's items number at least
`RANGE_FACTOR`, 64, times the cells that could hold them, and otherwise by one pass over the
group's rows. a5e7229b lets a bench replace the factor, so either route can be forced.

## What ran

The corpus of `probes/2026-10-10-batched-cell-counts/`: 20,000,000 items from the repository's
generator, seed 7, built at ee97910d without its annotation layers, one segment of view `s0`,
2.3 GB, in 169 s.

`viewport_sweep` ran three rounds, before and after alternating, with `--samples 5 --max-depth 8`,
over three views (the whole extent, a centred quarter and a centred sixteenth) at depths 0 to 8.
Its viewers:

| viewer | terms | items visible | share |
|---|---|---:|---:|
| narrow | one of 0 to 200 | 77,456 | 0.4% |
| medium | one of 0 to 200 | 155,795 | 0.8% |
| broad | one of 0 to 200 | 312,851 | 1.6% |
| 21 terms | 0 to 20 | 6,029,184 | 30% |
| everything | 0 to 200 | 19,730,510 | 98.7% |

A request figure is the median over rounds of each round's median of five requests, in wall time.
The count stage is each round's last request's. Before the change it is the per-tile counts summed
over the threads that ran them: a request of 4,096 tiles or more fans out over four threads, so
there it is thread time, not wall time. After, it is the one count before the fan-out, which runs
on one thread, plus the per-tile remainder, which is nothing without a filter or a highlight.

`aggregate_cost`, built at 41d6de9a, ran two rounds of the density of the whole view at depths 4 to
12 for four viewers, each forced by range (`--range-factor 0`) and by the pass
(`--range-factor 18446744073709551615`), `--threads 4 --repeat 3`. A figure is the median over
rounds of each round's median.

[`run.sh`](run.sh) is the sequence. Every run's output is in `runs/`; `python3 tiles.py` and
`python3 range_factor.py` print the tables below from it.

## Findings: the tile counts

**Every request served the same counts before and after:** the same visible total, non-empty
tiles and summed visible count in every run.

**A viewer whose set is stored as bitsets gains most.** For the 30% viewer the count stage takes
0.04 to 0.33 of its time before at depth 4 and deeper, and the request 0.57 to 0.94 at depth 2 and
deeper, 0.70 at the median. At depth 8 over the whole view the count falls from 382 ms to 14 ms and
the request from 529 ms to 399 ms.

**The complete viewer's count falls 1.3 to 9 times, and over the whole view at depths 4 to 8 its
request by 2 to 14%.** Its sets are long runs, cheaper to rank than bitsets, so it had less to
save: 104 ms to 14 ms at depth 8 over the whole view. Selection and the gather take most of its
request.

**For viewers of 0.4% to 1.6%, nothing measurable changes.** Their sets are array containers,
where croaring's count is two binary searches, as the shard probe found. The count stage takes 0.50
to 1.67 of its time before, and the request moves by up to 50% between rounds of the same binary on
this host, in selection and the gather, which the change does not touch. The request's medians lie
either side of 1.

**The first run after the change found a wrong count.** It served 33 of the narrow viewer's 77,456
items at depth 0. `count_ranges`, from 0d9348ff, counted a range from row 0 that ended past its
first container from the first container its call reached. A request's one tile at depth 0 is such
a range, and so is the first cell of a coarse aggregate. 41d6de9a corrects it, and every figure
here is from after that.

## Findings: the range factor

**`RANGE_FACTOR` stays at 64. No single factor picks the faster route for every viewer.** A count
by range costs about the same for every viewer at a depth, because it is a count per cell: 0.7 to
1.4 ms at depths 4 and 5, 1.7 to 2.6 ms at 6, 9.9 to 13.4 ms at 7, 25 to 30 ms at 8 and 9, 44 to 62
ms at 10, 146 to 205 ms at 11 and 430 to 530 ms at 12. The pass costs what the set's storage costs,
not its size: at depths 4 to 9 it takes 4.2 to 5.7 ms over 310,377 items in array containers, 30
to 33 ms over 6,029,184 in bitsets, and 6.2 to 10.5 ms over all 20,000,000, which are long runs.

So range counting wins down to 23 items per cell for the 30% viewer, and the pass wins up to 1,221
items per cell for the complete one. At 64 the faster route is chosen in 32 of the 36 cases. The
misses are the complete viewer at depths 7, 8 and 9 (9.9, 27.4 and 26.9 ms by range against 7.2, 7.4
and 10.5 ms by the pass) and the 30% viewer at depth 9 (33.4 ms by the pass against 30.0 by range).
A factor below 56.5 sends the 4.6% viewer at depth 7 to range counting, at 1.7 times the pass, and
below 18.9 the 1.6% viewer at depth 7, at 2.2 times. A factor above 1,221, which sends the complete
viewer at depths 7 to 9 to the pass, sends with it the 1.6% and 4.6% viewers at depths 4 to 6 and
the 30% viewer at depths 7 to 9, at 1.1 to 7.9 times their cost by range, and from 1,472 the 30%
viewer at depth 6, at 12.6 times.

A rule that sees the set's storage, values in run containers apart from the rest, could separate
the cases this one cannot. It is not built.

## Not measured

- A request with a filter or a highlight, whose matched and highlighted counts are still taken one
  part at a time.
- A bundle of several segments, or 10⁹ rows or more.
- More than four threads. The batched count runs on one thread before the fan-out, where the
  per-tile counts ran on every thread.
- The aggregate's other cell cases: a region, a field by cell, an artifact grouping.

## Tables

### Tile counts over the whole view, depths 4 to 8

Milliseconds. `python3 tiles.py` prints every view and depth.

| viewer | depth | tiles | count before | count after | request before | request after | after ÷ before |
|---|---:|---:|---:|---:|---:|---:|---:|
| narrow | 4 | 256 | 0.18 | 0.13 | 19.97 | 18.82 | 0.94 |
| narrow | 5 | 1,024 | 0.48 | 0.41 | 33.83 | 34.98 | 1.03 |
| narrow | 6 | 4,096 | 1.49 | 1.06 | 60.75 | 59.68 | 0.98 |
| narrow | 7 | 16,384 | 4.94 | 3.62 | 135.57 | 122.50 | 0.90 |
| narrow | 8 | 65,536 | 11.58 | 12.51 | 248.86 | 259.43 | 1.04 |
| medium | 4 | 256 | 0.24 | 0.17 | 30.59 | 25.60 | 0.84 |
| medium | 5 | 1,024 | 0.60 | 0.48 | 44.42 | 40.84 | 0.92 |
| medium | 6 | 4,096 | 1.75 | 1.27 | 67.75 | 64.27 | 0.95 |
| medium | 7 | 16,384 | 6.61 | 4.17 | 182.64 | 163.68 | 0.90 |
| medium | 8 | 65,536 | 16.90 | 14.80 | 338.77 | 334.04 | 0.99 |
| broad | 4 | 256 | 0.20 | 0.20 | 20.61 | 20.99 | 1.02 |
| broad | 5 | 1,024 | 0.60 | 0.46 | 50.69 | 50.13 | 0.99 |
| broad | 6 | 4,096 | 1.98 | 1.49 | 64.70 | 68.42 | 1.06 |
| broad | 7 | 16,384 | 5.09 | 4.49 | 176.29 | 192.15 | 1.09 |
| broad | 8 | 65,536 | 21.21 | 15.47 | 328.48 | 328.62 | 1.00 |
| 21 terms | 4 | 256 | 1.45 | 0.41 | 6.18 | 4.10 | 0.66 |
| 21 terms | 5 | 1,024 | 5.80 | 0.95 | 22.43 | 15.25 | 0.68 |
| 21 terms | 6 | 4,096 | 26.11 | 1.63 | 53.83 | 42.95 | 0.80 |
| 21 terms | 7 | 16,384 | 110.35 | 4.28 | 286.85 | 236.53 | 0.82 |
| 21 terms | 8 | 65,536 | 382.26 | 14.13 | 529.40 | 398.90 | 0.75 |
| everything | 4 | 256 | 0.54 | 0.23 | 4.28 | 3.85 | 0.90 |
| everything | 5 | 1,024 | 1.53 | 0.43 | 15.03 | 12.99 | 0.86 |
| everything | 6 | 4,096 | 5.90 | 1.16 | 39.82 | 39.17 | 0.98 |
| everything | 7 | 16,384 | 32.13 | 3.58 | 153.34 | 142.50 | 0.93 |
| everything | 8 | 65,536 | 103.56 | 13.88 | 559.87 | 543.15 | 0.97 |

### The density of the whole view, by range and by the pass

Milliseconds for the whole request. Cells are the depth's 4^depth, fewer than the view's
20,000,000 rows at every depth here. The viewers hold term 0, terms 0 to 2, terms 0 to 20 and every
term.

| viewer's items | depth | cells | items ÷ cells | by range | by the pass | range ÷ pass |
|---:|---:|---:|---:|---:|---:|---:|
| 310,377 | 4 | 256 | 1,212.4 | 0.71 | 5.20 | 0.14 |
| 310,377 | 5 | 1,024 | 303.1 | 0.88 | 4.83 | 0.18 |
| 310,377 | 6 | 4,096 | 75.8 | 1.74 | 4.22 | 0.41 |
| 310,377 | 7 | 16,384 | 18.9 | 10.64 | 4.87 | 2.19 |
| 310,377 | 8 | 65,536 | 4.7 | 25.44 | 4.54 | 5.60 |
| 310,377 | 9 | 262,144 | 1.2 | 30.00 | 5.69 | 5.27 |
| 310,377 | 10 | 1,048,576 | 0.3 | 52.80 | 8.36 | 6.32 |
| 310,377 | 11 | 4,194,304 | 0.1 | 203.09 | 9.97 | 20.37 |
| 310,377 | 12 | 16,777,216 | 0.0 | 497.14 | 8.74 | 56.90 |
| 925,543 | 4 | 256 | 3,615.4 | 0.90 | 6.54 | 0.14 |
| 925,543 | 5 | 1,024 | 903.9 | 0.88 | 6.94 | 0.13 |
| 925,543 | 6 | 4,096 | 226.0 | 2.30 | 7.39 | 0.31 |
| 925,543 | 7 | 16,384 | 56.5 | 13.37 | 7.80 | 1.71 |
| 925,543 | 8 | 65,536 | 14.1 | 27.65 | 8.01 | 3.45 |
| 925,543 | 9 | 262,144 | 3.5 | 27.49 | 8.82 | 3.12 |
| 925,543 | 10 | 1,048,576 | 0.9 | 61.97 | 11.44 | 5.42 |
| 925,543 | 11 | 4,194,304 | 0.2 | 205.04 | 12.43 | 16.49 |
| 925,543 | 12 | 16,777,216 | 0.1 | 530.41 | 17.66 | 30.04 |
| 6,029,184 | 4 | 256 | 23,551.5 | 1.38 | 29.67 | 0.05 |
| 6,029,184 | 5 | 1,024 | 5,887.9 | 1.36 | 31.31 | 0.04 |
| 6,029,184 | 6 | 4,096 | 1,472.0 | 2.58 | 32.45 | 0.08 |
| 6,029,184 | 7 | 16,384 | 368.0 | 12.91 | 32.49 | 0.40 |
| 6,029,184 | 8 | 65,536 | 92.0 | 29.12 | 33.43 | 0.87 |
| 6,029,184 | 9 | 262,144 | 23.0 | 29.96 | 33.44 | 0.90 |
| 6,029,184 | 10 | 1,048,576 | 5.7 | 58.15 | 49.42 | 1.18 |
| 6,029,184 | 11 | 4,194,304 | 1.4 | 205.20 | 53.23 | 3.85 |
| 6,029,184 | 12 | 16,777,216 | 0.4 | 429.46 | 58.57 | 7.33 |
| 20,000,000 | 4 | 256 | 78,125.0 | 0.85 | 7.30 | 0.12 |
| 20,000,000 | 5 | 1,024 | 19,531.2 | 0.88 | 6.20 | 0.14 |
| 20,000,000 | 6 | 4,096 | 4,882.8 | 2.40 | 6.66 | 0.36 |
| 20,000,000 | 7 | 16,384 | 1,220.7 | 9.88 | 7.21 | 1.37 |
| 20,000,000 | 8 | 65,536 | 305.2 | 27.38 | 7.42 | 3.69 |
| 20,000,000 | 9 | 262,144 | 76.3 | 26.93 | 10.54 | 2.55 |
| 20,000,000 | 10 | 1,048,576 | 19.1 | 44.21 | 15.98 | 2.77 |
| 20,000,000 | 11 | 4,194,304 | 4.8 | 145.81 | 28.62 | 5.09 |
| 20,000,000 | 12 | 16,777,216 | 1.2 | 462.62 | 58.00 | 7.98 |
