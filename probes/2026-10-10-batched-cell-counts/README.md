# Counting a segment's cells in one walk: the density underlay and aggregate cells, before and after

**Status:** measurement, 2026-10-10. Before: the engine and `mosaica-roaring` at a5c46c2f. After:
0d9348ff, which counts a segment's cells with `mosaica_roaring::count_ranges`. Host: a cloud
container on an Intel Xeon at 2.80 GHz, 4 cores, 15 GB, with nothing else running. Every figure
is measured.

## What changed

The density underlay of a viewport request and the aggregate's cell counts by range both count
a viewer's set over each occupied cell of each segment (`cells::count_by_ranges`). Each cell was
counted by `EffectiveMask::count_range`, two ranks per bitmap, each a popcount from the start of
the cell's container. After the change the cells of one segment are counted together: every
range's two ends are ranked in one call to CRoaring's `roaring_bitmap_rank_many`, which carries the
rank forward through each container, so the cells sharing a container popcount it once between
them.

The tile counts of a viewport request are counted one range at a time, as before.

## What ran

A corpus of 20,000,000 items from the repository's generator, built without its annotation
layers, which neither path reads:

```text
mosaica corpus materialise --seed 7 --n 20000000 --out src
# points-only.toml: src/corpus-config.toml up to its first [[layer]]
mosaica build --memory-budget 10g          # [build] schema = "points-only.toml"
```

The bundle is one segment of 20,000,000 rows in view `s0`, 1,025 access terms, 2.3 GB, built in
2 min 28 s.

`underlay_route` (`crates/mosaica-bench/src/bin/underlay_route.rs`) times the underlay loop
(`underlay_ns`) and the whole request for two viewers: `medium`, who holds one term and sees
155,795 items (0.8%), and `everything`, who holds terms 0 to 200 and sees 19,730,510 (98.7%). The
loop's time is summed over the request's threads, so it can exceed the request's wall time. Each
binary ran three times, alternating, five requests per case:

```text
underlay_route --fixture src/bundle --samples 5 --max-offset 9   # run 1
underlay_route --fixture src/bundle --samples 5 --max-offset 5   # runs 2 and 3
```

`aggregate_cost` (`crates/mosaica-bench/src/bin/aggregate_cost.rs`) ran the cell cases twice per
binary, five requests each, for a principal holding every term:

```text
aggregate_cost --bundle src/bundle --view s0 --field bay --bins weight --by-cell bay \
  --polygon '0.40,0.40;0.62,0.40;0.62,0.61;0.40,0.61' --threads 4 --repeat 5 \
  --case density --case cells
```

The polygon holds no item of this corpus, so only the whole-view cases are reported. Every run's
output is in `runs/`; `python3 tables.py` prints the tables below from it. The cells each request
emitted were the same before and after in every case.

## Findings

**For a viewer who sees most of the map, the underlay is 1.4 to 10 times faster and the request
1.2 to 6.6 times.** At depth 7 and offset 5, 11.7 million sub-cells over 16,384 tiles, the loop
took 7.6 s of thread time before and 0.75 s after, and the request 2.46 s and 0.62 s. Under one
tile at depth 6, the loop is 16 to 20 times faster at offsets 4 and 5.

**For a viewer who sees 0.8% of it, the change is within this host's noise.** The loop runs at
0.88 to 1.49 times its old speed and the request at 0.89 to 1.29. This viewer's projection holds
array containers, where a range count is two binary searches, so there is little popcount to
share.

**The first version was slower where a tile holds few cells.** `roaring_bitmap_rank_many` adds up
the cardinality of every container below the first value it ranks, and the underlay makes one
call per tile, so each call walked the bitmap from its start: about 300 containers here, and tens
of thousands at a shard of 2³² rows. At offset 2, 16 cells a tile, its loop took 124.7 ms at depth
6 and 337.3 ms at depth 7, against 93.5 ms and 262.3 ms before in the same run (the last column of
the first table). `count_ranges` now ranks over a view of only the containers the ranges reach, and
the same cases take 55.9 ms and 102.2 ms.

**The aggregate's whole-view density at depth 6 is about 1.3 times faster.** It counts 4,096 cells
by range, each about one container wide, so the cells share few containers: 1.82 to 2.34 ms before
and 1.34 to 1.54 ms after. Its other cell cases take the pass over the set's rows, which the
change does not touch; their spread between runs, up to 45%, is this host's.

## A correction

0d9348ff's view counted a range from row 0 wrongly where the range ended past its first
container: with no lower rank to subtract, its count was its end's rank, taken from the first
container the call reached rather than from the first container of the bitmap. A one-tile request
over this corpus counted 33 of a viewer's 77,456 items. A call holding such a range now ranks from
the bitmap's first container. No case above holds one: every cell from row 0 ends inside the first
container.

## Not measured

- A bundle of several segments, where a tile's cells are counted once per segment.
- A bundle of 10⁹ rows or more.
- Whether the aggregate's `RANGE_FACTOR`, the row count a range count is charged as when the
  aggregate chooses between ranges and the pass, should fall now that a range count costs less.
  The bench cannot force either route.

## Tables

### Density underlay over the whole view

Median of three runs, ms. The underlay figure of a run is its last request's; the request figure
is the median of its five. The last column is the first version, which ranked from the bitmap's
start, from run 1 alone.

| viewer | depth | offset | underlay before | underlay after | ratio | request before | request after | ratio | underlay, first version |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| medium | 5 | 2 | 21.66 | 22.13 | 0.98 | 59.9 | 61.9 | 0.97 | 23.66 |
| medium | 5 | 3 | 44.51 | 45.39 | 0.98 | 84.7 | 89.3 | 0.95 | 46.57 |
| medium | 5 | 4 | 60.80 | 62.68 | 0.97 | 100.6 | 112.9 | 0.89 | 63.53 |
| medium | 5 | 5 | 118.68 | 103.67 | 1.14 | 159.8 | 147.9 | 1.08 | 103.10 |
| medium | 6 | 2 | 57.19 | 55.37 | 1.03 | 76.6 | 75.6 | 1.01 | 51.72 |
| medium | 6 | 3 | 80.44 | 74.67 | 1.08 | 79.0 | 80.4 | 0.98 | 80.94 |
| medium | 6 | 4 | 160.88 | 136.81 | 1.18 | 113.9 | 92.4 | 1.23 | 126.44 |
| medium | 6 | 5 | 479.90 | 364.15 | 1.32 | 194.4 | 163.8 | 1.19 | 343.11 |
| medium | 7 | 2 | 82.81 | 93.94 | 0.88 | 177.9 | 179.0 | 0.99 | 87.69 |
| medium | 7 | 3 | 186.04 | 152.44 | 1.22 | 206.4 | 197.7 | 1.04 | 153.03 |
| medium | 7 | 4 | 488.81 | 365.35 | 1.34 | 288.2 | 243.6 | 1.18 | 393.39 |
| medium | 7 | 5 | 1,352.26 | 908.24 | 1.49 | 488.5 | 380.0 | 1.29 | 901.42 |
| everything | 5 | 2 | 31.74 | 23.01 | 1.38 | 45.4 | 37.2 | 1.22 | 38.20 |
| everything | 5 | 3 | 84.42 | 47.01 | 1.80 | 99.2 | 60.3 | 1.65 | 76.56 |
| everything | 5 | 4 | 231.99 | 60.90 | 3.81 | 227.4 | 76.3 | 2.98 | 88.43 |
| everything | 5 | 5 | 711.85 | 86.27 | 8.25 | 718.0 | 108.1 | 6.64 | 103.29 |
| everything | 6 | 2 | 93.53 | 55.88 | 1.67 | 71.0 | 53.8 | 1.32 | 124.66 |
| everything | 6 | 3 | 227.81 | 75.38 | 3.02 | 104.2 | 61.3 | 1.70 | 183.52 |
| everything | 6 | 4 | 838.12 | 105.34 | 7.96 | 236.2 | 75.6 | 3.13 | 197.67 |
| everything | 6 | 5 | 2,783.32 | 303.16 | 9.18 | 784.6 | 163.9 | 4.79 | 363.12 |
| everything | 7 | 2 | 254.05 | 102.15 | 2.49 | 213.6 | 184.3 | 1.16 | 337.25 |
| everything | 7 | 3 | 737.92 | 145.90 | 5.06 | 363.2 | 194.6 | 1.87 | 408.51 |
| everything | 7 | 4 | 2,839.74 | 293.78 | 9.67 | 897.9 | 268.8 | 3.34 | 601.80 |
| everything | 7 | 5 | 7,554.19 | 751.65 | 10.05 | 2,461.3 | 622.2 | 3.96 | 1,143.98 |

A request at depth 5 covers 1,024 tiles, at depth 6 4,096 and at depth 7 16,384. Offset `o` asks
for 4ᵒ sub-cells a tile.

### Density underlay under one tile

| viewer | depth | offset | underlay before | underlay after | ratio | request before | request after | ratio |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| medium | 2 | 4 | 0.04 | 0.04 | 1.02 | 0.2 | 0.2 | 1.17 |
| medium | 2 | 5 | 0.40 | 0.39 | 1.02 | 0.7 | 0.7 | 1.03 |
| medium | 4 | 4 | 0.03 | 0.03 | 1.03 | 0.1 | 0.1 | 1.01 |
| medium | 4 | 5 | 0.12 | 0.11 | 1.13 | 0.2 | 0.2 | 1.21 |
| medium | 6 | 4 | 0.02 | 0.02 | 1.33 | 0.0 | 0.0 | 1.18 |
| medium | 6 | 5 | 0.09 | 0.07 | 1.38 | 0.1 | 0.1 | 1.30 |
| everything | 2 | 4 | 0.17 | 0.07 | 2.52 | 0.2 | 0.1 | 2.21 |
| everything | 2 | 5 | 0.92 | 0.40 | 2.27 | 1.0 | 0.5 | 2.15 |
| everything | 4 | 4 | 0.16 | 0.03 | 6.24 | 0.2 | 0.0 | 4.40 |
| everything | 4 | 5 | 0.62 | 0.08 | 7.48 | 0.7 | 0.1 | 6.56 |
| everything | 6 | 4 | 0.26 | 0.02 | 16.25 | 0.3 | 0.0 | 8.45 |
| everything | 6 | 5 | 1.07 | 0.05 | 20.19 | 1.1 | 0.1 | 15.94 |

### Aggregate cell counts, whole view

Median ms of five requests, per run.

| case | route | before, run 1 | before, run 2 | after, run 1 | after, run 2 |
|---|---|---:|---:|---:|---:|
| density d6 | ranges | 2.34 | 1.82 | 1.34 | 1.54 |
| density d16 | pass | 50.91 | 51.03 | 73.87 | 53.79 |
| density d32 | pass | 80.02 | 87.06 | 106.35 | 85.31 |
| bay top 10 x cells d16 | pass | 223.37 | 198.07 | 200.10 | 203.35 |
