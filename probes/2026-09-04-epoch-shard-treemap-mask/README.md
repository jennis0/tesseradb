# What an epoch-shard treemap mask costs the viewport sweep

**Status:** measurement, 2026-09-04. Synthetic and in memory, one thread, on the 12-core WSL2
box. Harness: `cargo run --release -p tessera-bench --bin epoch_shard_treemap_mask`. Every cell
with its five samples is in [`result.json`](result.json); the raw µs tables at every N are in
[`tables.md`](tables.md).

**Over 2× for every operation at N = 8, and close to N× once tiles are small.** Each of the
sweep's three mask operations has a fixed cost per part — a rank inside a Roaring container, an
iterator seek, a tier dispatch — and a tile of N parts pays it N times however few rows the parts
hold. Where a tile holds enough rows for per-row work to dominate, N makes no difference (ratios
0.9–1.1, within noise). The depth from which the N = 8 / N = 1 ratio is at or above 2, for a
request of 256 tiles, by mask coverage:

| operation | 50% | 10% | 1% | 0.1% | run-heavy 10% | per 256-tile request at depth 12, N = 1 → N = 8 |
|---|---|---|---|---|---|---|
| count | depth 3 | 2 | 3 | 4 | 3 | 0.49 → 4.5 ms (50%); 0.42 → 4.0 ms (10%); 21 → 291 µs (1%); 15 → 179 µs (0.1%) |
| decode | 10 | 9 | 7 | 6 | 10 | 56 → 443 µs; 32 → 386 µs; 33 → 378 µs; 28 → 342 µs |
| rows_in_range | 10 | 5 | 5 | 5 | 10 | 57 → 476 µs; 44 → 477 µs; 51 → 501 µs; 36 → 557 µs |
| select | 11 | 11 | 9 | 7 | 9 | 265 → 780 µs; 77 → 524 µs; 45 → 392 µs; 32 → 394 µs |

The count crosses first and costs most. On a bitset container (coverage above about 6%)
`range_cardinality` over a range inside one container is two ranks, each a popcount from the
container's start: 1.6–1.9 µs per call with the container cold, and the cost does not fall with the
range's length. On an array container it is two binary searches at 60–80 ns. A 256-tile request at
N = 8 makes 2,048 such calls: **4–5.5 ms against 0.4–0.9 ms today at every depth from 4 down for a
50% or 10% principal**, 0.15–0.7 ms against 13–80 µs for a 1% or 0.1% one. `tile_sweep` calls
`count_range` twice per part (`matched` and `visible`), so the engine's figure is twice this
column's.

Decode and select are unchanged to depth 5 or 6 at every coverage, where a request visits 10⁸ to
5×10⁸ visible rows and the per-row work hides the parts. By depth 12 a part holds eight rows or
fewer, each of the 2,048 parts costs 170–380 ns of set-up and seek, and the ratio is 8–12× on a
request that costs 30–270 µs today.

Put together (modelled from the columns, not measured as one request): at N = 8 the mask work of a
256-tile request at depth 5 or deeper is about 5–6 ms single-threaded for a dense principal, nearly
all of it the count, and about 1 ms for a sparse one — against about 0.6 ms and 0.1 ms today.

## The question

Split a corpus into N epoch shards inside one process. The viewer mask becomes a treemap: a sorted
vector of `(shard id, croaring 32-bit Bitmap)`, a row addressed as shard in the high word and a
local `u32` in the low word. A tile that is one contiguous row range today becomes N ranges, one
per shard, each about 1/N as long. What does that cost the viewport sweep's three mask operations
at N ∈ {1, 2, 4, 8}?

## What was measured

The three operations as `viewport.rs`'s `tile_sweep` uses them, over a `Treemap` type written in
the bin (a sorted `Vec<(u32, Bitmap)>` whose every method finds the leaf and makes the croaring
call `EffectiveMask` makes today on its one bitmap; under 100 lines), plus one column for the
route the overlay diffs would force:

1. **count** — `RowProjection::range_cardinality` over each part of each tile.
2. **decode** — each part's visible rows read the way `Selection::of` reads them: the tier from
   `select::decode_tier(visible, range.len())`, then the full range, a cursor over runs
   (`compose.rs`'s `for_each_run_in`, transcribed because it is private) or the batched
   `reset_at_or_after` / `next_many` value walk, on the diffs-empty route the steady state takes.
3. **select** — `Selection::of` itself, unchanged and reached through `tessera_engine::select`
   (no re-export was needed), with N `SelectionPart`s per tile: cap 30, `k_min` 4, θ anchored on
   the composed total at `theta_target_marks` 16 and shifted by depth.
4. **rows_in_range** — `EffectiveMask::rows_in_range` with the diffs empty, `leaf ∩ range`
   materialised: what decode pays instead when the overlay diffs are non-empty.

**Masks.** A universe of 2³⁰ rows: one bitmap over it at N = 1, N leaves over 2³⁰/N rows each
otherwise, at the same density. Rows are chosen independently at random at 50%, 10%, 1% and 0.1%
(realistic masks measure as essentially scattered under Morton order, run ratio 1.03–1.15 —
`probes/results.md` §5), giving bitset containers at 50% and 10% and array containers at 1% and
0.1%. One run-heavy variant at 10% for contrast: geometric runs of mean 256 rows and gaps of mean
2,304, which is bitset containers with 6% array containers. Every leaf is built through `add_many`
and not run-optimised, as a row projection is; the container statistics of every leaf are in
`result.json`.

**Tiles.** At depth d a tile is 2³⁰/4ᵈ rows in the single space and N ranges of 2³⁰/(N·4ᵈ) rows in
the leaves, at the same map position. Each part's start is moved by up to one container (65,536
rows) of random jitter, bounded so the part stays inside its leaf: a real tile's range comes from
`partition_point` over the Morton column and begins at an arbitrary row, whereas 2³⁰/4ᵈ is a
multiple of the container width up to depth 7, and without the jitter the N = 1 count was read from
container headers alone — an advantage no real tile has and one the split then appeared to lose. A
request is 256 distinct tiles at random positions from depth 4; at depths 0–3 it is every tile at
that depth (1, 4, 16, 64). The same positions serve every N. Depths 0–14; depth 14 is skipped at
N = 8, where a part would be half a row.

**Order of work.** The count pass runs once per depth outside the timers and its per-part results
feed decode and select, as `tile_sweep`'s count feeds its selection.

**Selection's inputs.** `Selection::of` takes one `EffectiveMask` over one row space, so for the
select column the N leaves are presented to it as a single view-space bitmap — leaf s at row base
s·2³⁰/N — wrapped in a real `EffectiveMask` through `compose` with empty diffs, and the tile as N
parts at their view-space ranges. That is the parts-and-ranges shape a treemap-backed mask would
hand the shipped loop; what it omits is the leaf lookup, a binary search over at most eight
entries, which the count and decode columns include. The identity column is 2²⁶ random `u64`s
(512 MB) in a segment written through `write_columns_from_parts`; each part reads it at its own
random offset so consecutive tiles do not share cache lines. Select is therefore measured from
depth 2, the first depth whose single-space tile fits the column; depths 0 and 1 would need a 2 GB
and an 8 GB column.

**Timing.** One warm-up request then five timed, median reported, all five kept. One thread: no
pool is created and croaring is single-threaded. `nice -n 10`. AMD Ryzen 9 5900X, 12 cores, 47 GB,
WSL2; the process was 0.4 GB resident early in the run and its largest case holds two 128 MB
bitmaps and maps a 768 MB column (bounded, not measured at peak). The run took about 13 minutes.
Another agent's rung 5 ingest ran on the box throughout, load average 7–11 — see the caveats.

## The tables

Median µs per request, N = 1 → N = 8, with the ratio; bold where the ratio is at or above 2. The
same tables with the N = 2 and N = 4 columns are in [`tables.md`](tables.md).

#### Count: `range_cardinality` per part

| depth | rows/tile at N=1 | 50% | 10% | 1% | 0.1% | run-heavy 10% |
|---|---|---|---|---|---|---|
| 0 | 1073741824 | 82.0 → 94.6 (1.2×) | 129 → 133 (1.0×) | 68.8 → 64.8 (0.9×) | 40.1 → 29.6 (0.7×) | 80.9 → 92.7 (1.1×) |
| 1 | 268435456 | 110 → 173 (1.6×) | 89.3 → 141 (1.6×) | 89.8 → 70.0 (0.8×) | 31.5 → 44.4 (1.4×) | 88.6 → 137 (1.5×) |
| 2 | 67108864 | 243 → 303 (1.2×) | 105 → 304 (**2.9×**) | 86.1 → 87.0 (1.0×) | 31.7 → 41.5 (1.3×) | 167 → 285 (1.7×) |
| 3 | 16777216 | 337 → 1001 (**3.0×**) | 217 → 1156 (**5.3×**) | 70.2 → 173 (**2.5×**) | 85.3 → 118 (1.4×) | 218 → 885 (**4.1×**) |
| 4 | 4194304 | 906 → 3986 (**4.4×**) | 564 → 3752 (**6.7×**) | 79.8 → 588 (**7.4×**) | 56.0 → 292 (**5.2×**) | 475 → 3720 (**7.8×**) |
| 5 | 1048576 | 555 → 5627 (**10.1×**) | 490 → 4806 (**9.8×**) | 59.3 → 711 (**12.0×**) | 32.2 → 201 (**6.2×**) | 648 → 5018 (**7.7×**) |
| 6 | 262144 | 758 → 5287 (**7.0×**) | 518 → 4548 (**8.8×**) | 33.6 → 622 (**18.5×**) | 17.3 → 174 (**10.1×**) | 524 → 4850 (**9.3×**) |
| 7 | 65536 | 509 → 5085 (**10.0×**) | 464 → 4231 (**9.1×**) | 34.6 → 719 (**20.8×**) | 15.1 → 189 (**12.5×**) | 1007 → 7277 (**7.2×**) |
| 8 | 16384 | 600 → 4495 (**7.5×**) | 435 → 4118 (**9.5×**) | 28.0 → 552 (**19.7×**) | 17.5 → 146 (**8.3×**) | 434 → 4112 (**9.5×**) |
| 9 | 4096 | 910 → 4513 (**5.0×**) | 456 → 4245 (**9.3×**) | 28.7 → 286 (**10.0×**) | 12.8 → 188 (**14.7×**) | 407 → 4068 (**10.0×**) |
| 10 | 1024 | 519 → 5545 (**10.7×**) | 429 → 4030 (**9.4×**) | 21.5 → 276 (**12.9×**) | 14.6 → 196 (**13.5×**) | 505 → 4070 (**8.1×**) |
| 11 | 256 | 497 → 4592 (**9.2×**) | 388 → 3965 (**10.2×**) | 21.5 → 271 (**12.6×**) | 12.8 → 180 (**14.1×**) | 782 → 3910 (**5.0×**) |
| 12 | 64 | 490 → 4543 (**9.3×**) | 418 → 4012 (**9.6×**) | 21.3 → 291 (**13.7×**) | 14.6 → 179 (**12.2×**) | 826 → 4149 (**5.0×**) |
| 13 | 16 | 466 → 12097 (**26.0×**) | 410 → 4020 (**9.8×**) | 22.9 → 278 (**12.1×**) | 13.0 → 193 (**14.9×**) | 863 → 4194 (**4.9×**) |

#### Decode: the tier path per part

| depth | rows/tile at N=1 | 50% | 10% | 1% | 0.1% | run-heavy 10% |
|---|---|---|---|---|---|---|
| 0 | 1073741824 | 1003660 → 923776 (0.9×) | 250850 → 242064 (1.0×) | 10564 → 9945 (0.9×) | 1074 → 1137 (1.1×) | 183383 → 187460 (1.0×) |
| 1 | 268435456 | 2069085 → 939727 (0.5×) | 277744 → 241126 (0.9×) | 10345 → 16424 (1.6×) | 1057 → 2469 (**2.3×**) | 203834 → 186238 (0.9×) |
| 2 | 67108864 | 1964147 → 950914 (0.5×) | 266924 → 244156 (0.9×) | 21653 → 10045 (0.5×) | 1066 → 1137 (1.1×) | 258165 → 190789 (0.7×) |
| 3 | 16777216 | 1087097 → 972740 (0.9×) | 757147 → 236047 (0.3×) | 9987 → 10187 (1.0×) | 1083 → 1186 (1.1×) | 204245 → 182081 (0.9×) |
| 4 | 4194304 | 984974 → 911296 (0.9×) | 238750 → 239910 (1.0×) | 9918 → 10362 (1.0×) | 1085 → 1375 (1.3×) | 181842 → 174341 (1.0×) |
| 5 | 1048576 | 263188 → 237025 (0.9×) | 59200 → 61791 (1.0×) | 2600 → 3334 (1.3×) | 302 → 591 (2.0×) | 298614 → 53927 (0.2×) |
| 6 | 262144 | 60489 → 162225 (**2.7×**) | 14598 → 16931 (1.2×) | 663 → 1215 (1.8×) | 99.3 → 368 (**3.7×**) | 12789 → 13698 (1.1×) |
| 7 | 65536 | 15253 → 21293 (1.4×) | 3759 → 7118 (1.9×) | 221 → 1123 (**5.1×**) | 48.5 → 307 (**6.3×**) | 6073 → 4919 (0.8×) |
| 8 | 16384 | 3967 → 4439 (1.1×) | 938 → 1638 (1.7×) | 99.0 → 434 (**4.4×**) | 33.8 → 283 (**8.4×**) | 771 → 2169 (**2.8×**) |
| 9 | 4096 | 1886 → 1469 (0.8×) | 257 → 812 (**3.2×**) | 45.7 → 416 (**9.1×**) | 28.6 → 348 (**12.1×**) | 465 → 892 (1.9×) |
| 10 | 1024 | 302 → 1307 (**4.3×**) | 83.5 → 676 (**8.1×**) | 45.7 → 389 (**8.5×**) | 29.2 → 364 (**12.5×**) | 105 → 743 (**7.1×**) |
| 11 | 256 | 110 → 483 (**4.4×**) | 44.5 → 408 (**9.2×**) | 38.1 → 378 (**9.9×**) | 28.3 → 334 (**11.8×**) | 121 → 915 (**7.6×**) |
| 12 | 64 | 55.8 → 443 (**8.0×**) | 32.0 → 386 (**12.1×**) | 32.6 → 378 (**11.6×**) | 28.1 → 342 (**12.2×**) | 97.0 → 1080 (**11.1×**) |
| 13 | 16 | 50.1 → 338 (**6.8×**) | 45.3 → 374 (**8.3×**) | 37.7 → 380 (**10.1×**) | 28.4 → 357 (**12.6×**) | 88.8 → 645 (**7.3×**) |

#### Rows in range: materialised `leaf ∩ range` per part

| depth | rows/tile at N=1 | 50% | 10% | 1% | 0.1% | run-heavy 10% |
|---|---|---|---|---|---|---|
| 0 | 1073741824 | 17932 → 12504 (0.7×) | 16557 → 10868 (0.7×) | 5453 → 3676 (0.7×) | 1739 → 1851 (1.1×) | 15855 → 12815 (0.8×) |
| 1 | 268435456 | 24217 → 9740 (0.4×) | 15467 → 9966 (0.6×) | 4142 → 3813 (0.9×) | 1737 → 1872 (1.1×) | 14093 → 9578 (0.7×) |
| 2 | 67108864 | 9032 → 9931 (1.1×) | 9795 → 10372 (1.1×) | 3577 → 3828 (1.1×) | 1787 → 2102 (1.2×) | 9450 → 11019 (1.2×) |
| 3 | 16777216 | 9290 → 10396 (1.1×) | 12400 → 11557 (0.9×) | 3770 → 3694 (1.0×) | 1801 → 1273 (0.7×) | 9834 → 10750 (1.1×) |
| 4 | 4194304 | 9748 → 10876 (1.1×) | 10313 → 18409 (1.8×) | 3759 → 4689 (1.2×) | 1993 → 1416 (0.7×) | 11057 → 25642 (**2.3×**) |
| 5 | 1048576 | 3482 → 6543 (1.9×) | 3769 → 13594 (**3.6×**) | 750 → 2552 (**3.4×**) | 295 → 752 (**2.5×**) | 4799 → 11220 (**2.3×**) |
| 6 | 262144 | 1009 → 4525 (**4.5×**) | 2590 → 13544 (**5.2×**) | 310 → 1230 (**4.0×**) | 121 → 478 (**4.0×**) | 1554 → 8150 (**5.2×**) |
| 7 | 65536 | 679 → 11220 (**16.5×**) | 1298 → 5706 (**4.4×**) | 247 → 1285 (**5.2×**) | 69.6 → 394 (**5.7×**) | 942 → 4758 (**5.1×**) |
| 8 | 16384 | 453 → 3433 (**7.6×**) | 936 → 3423 (**3.7×**) | 106 → 551 (**5.2×**) | 46.7 → 358 (**7.7×**) | 731 → 3383 (**4.6×**) |
| 9 | 4096 | 767 → 1103 (1.4×) | 664 → 1331 (**2.0×**) | 66.6 → 522 (**7.8×**) | 42.0 → 586 (**14.0×**) | 1074 → 1045 (1.0×) |
| 10 | 1024 | 236 → 597 (**2.5×**) | 194 → 575 (**3.0×**) | 56.1 → 518 (**9.2×**) | 38.2 → 529 (**13.9×**) | 250 → 581 (**2.3×**) |
| 11 | 256 | 91.5 → 462 (**5.0×**) | 74.6 → 457 (**6.1×**) | 50.4 → 492 (**9.8×**) | 36.4 → 528 (**14.5×**) | 157 → 559 (**3.6×**) |
| 12 | 64 | 56.6 → 476 (**8.4×**) | 44.4 → 477 (**10.7×**) | 51.0 → 501 (**9.8×**) | 36.1 → 557 (**15.4×**) | 95.4 → 471 (**4.9×**) |
| 13 | 16 | 49.0 → 438 (**8.9×**) | 47.1 → 427 (**9.1×**) | 52.4 → 491 (**9.4×**) | 41.3 → 539 (**13.1×**) | 84.8 → 429 (**5.1×**) |

#### Select: `Selection::of`, N parts per tile

| depth | rows/tile at N=1 | 50% | 10% | 1% | 0.1% | run-heavy 10% |
|---|---|---|---|---|---|---|
| 2 | 67108864 | 1462139 → 1303172 (0.9×) | 602988 → 640472 (1.1×) | 146834 → 148793 (1.0×) | 18274 → 16470 (0.9×) | 315287 → 324600 (1.0×) |
| 3 | 16777216 | 1489616 → 1274829 (0.9×) | 612803 → 593812 (1.0×) | 146251 → 148099 (1.0×) | 16606 → 17082 (1.0×) | 307468 → 314774 (1.0×) |
| 4 | 4194304 | 1648706 → 1381816 (0.8×) | 578947 → 599127 (1.0×) | 155165 → 154810 (1.0×) | 17606 → 18610 (1.1×) | 342053 → 322621 (0.9×) |
| 5 | 1048576 | 398113 → 543783 (1.4×) | 150011 → 149629 (1.0×) | 38559 → 39704 (1.0×) | 5164 → 5929 (1.1×) | 513939 → 82596 (0.2×) |
| 6 | 262144 | 96676 → 193620 (**2.0×**) | 36590 → 41862 (1.1×) | 10767 → 12041 (1.1×) | 1731 → 2345 (1.4×) | 162566 → 23748 (0.1×) |
| 7 | 65536 | 24625 → 31032 (1.3×) | 10235 → 12356 (1.2×) | 3317 → 4359 (1.3×) | 447 → 897 (**2.0×**) | 6682 → 8910 (1.3×) |
| 8 | 16384 | 7292 → 8653 (1.2×) | 3226 → 4919 (1.5×) | 970 → 1385 (1.4×) | 139 → 628 (**4.5×**) | 23565 → 4397 (0.2×) |
| 9 | 4096 | 2866 → 4533 (1.6×) | 1224 → 2354 (1.9×) | 314 → 764 (**2.4×**) | 56.4 → 455 (**8.1×**) | 887 → 1937 (**2.2×**) |
| 10 | 1024 | 1345 → 2055 (1.5×) | 564 → 1075 (1.9×) | 138 → 623 (**4.5×**) | 35.4 → 385 (**10.9×**) | 548 → 1881 (**3.4×**) |
| 11 | 256 | 688 → 1434 (**2.1×**) | 197 → 658 (**3.3×**) | 50.3 → 430 (**8.6×**) | 35.2 → 352 (**10.0×**) | 218 → 773 (**3.5×**) |
| 12 | 64 | 265 → 780 (**2.9×**) | 76.6 → 524 (**6.8×**) | 45.1 → 392 (**8.7×**) | 32.2 → 394 (**12.2×**) | 169 → 771 (**4.6×**) |
| 13 | 16 | 129 → 602 (**4.7×**) | 49.9 → 413 (**8.3×**) | 37.6 → 409 (**10.9×**) | 33.2 → 386 (**11.6×**) | 153 → 861 (**5.6×**) |

## What it says

**The cost is a fixed cost per part, multiplied by N.** At depth 12 the count costs 1.9 µs per
tile at N = 1 and 2.2 µs per *part* at N = 8 on the 50% mask (83 and 142 ns on the 1% mask); the
decode costs 110–220 ns per tile at N = 1 and 170–220 ns per part at N = 8; select 130–1,000 ns
per tile and 190–380 ns per part. None of these falls as the part shortens, so a tile of N parts
costs N times a tile of one, less whatever per-row work still amortises it. The count has no
per-row component at all — it is O(containers touched) — so nothing amortises the multiplication
except a tile spanning many whole containers, which is why it crosses 2× by depth 2–4 while decode
and select, which read every visible row, cross at depth 6–11 depending on how few rows a tile has.

**Which principals pay.** Dense principals pay the most in absolute terms, because their containers
are bitsets and a rank inside a bitset is a popcount from the container's start: 4–5.5 ms of count
per 256 tiles at N = 8. Sparse principals cross earliest on decode and select, because their
per-row work is the smallest and the per-part cost shows through sooner; their absolute cost is a
few hundred microseconds per request.

**Shallow depths are free.** Where a tile spans thousands of containers (depths 0–2) or a request
visits 10⁸ rows (depths 0–5 for decode and select), the ratios sit at 0.9–1.1. Those cells are
noise around unity, not a saving.

**The engine already has the N-part shape.** `tile_sweep` takes a tile as a list of `(segment,
range)` parts, so a view whose tile straddles the build segment and a flush segment pays these
per-part costs today. The treemap would make every view an N-part view at every tile.

**Not covered, and how it would move the figures.** The leaf lookup for select is omitted (a
binary search over at most eight entries; the count and decode columns include it and are the
ratio's upper bound). Overlay diffs are absent: `count_range` adds two `range_cardinality` calls
over the small `minus` and `plus` bitmaps per part, which a treemap would presumably also shard,
and a non-empty diff sends decode down the rows_in_range column instead. The run tier (density at
or above 95%) fires in no variant beyond the occasional fully-visible tiny tile.

## Caveats

- **Noise from a shared box.** Another agent's ingest ran throughout (load average 7–11 on 12
  cores). Medians of five absorb single spikes but not a sustained load that lands on one N and not
  another, and the N values were measured in sequence. Recomputed on the minimum of the five
  samples rather than the median, every crossing depth in the summary table is the same or one
  level different (count at 0.1% one level shallower; select at 50% and 0.1% one level deeper),
  and the depth-9 dip in rows_in_range at 10% falls from 2.0× to 1.6×. Six N = 1 cells have a
  median over twice their own minimum — decode at depth 2 (50% and 1%), run-heavy decode at depth
  5 and run-heavy select at depths 5, 6 and 8 — and those understate the ratio: the run-heavy
  select ratios of 0.1–0.2× at depths 5–8 should be read as about 1×.
- **rows_in_range is not monotone in depth**: 16.5× at 50% depth 7 and 4.5–7.6× at depths 6–8,
  then 1.0–1.4× at depth 9 for the 50% and run-heavy masks. The AND's result changes container
  type with the part's cardinality (the array/bitset threshold is 4,096 values), and the cost of
  producing it moves with the type; not separated here.
- **One unexplained baseline.** The run-heavy count at N = 1 doubles to 0.8 ms per request from
  depth 11 (0.4–0.5 ms at its shallower depths and for the scattered 10% mask throughout), which
  halves that variant's ratio there to 5×. It is consistent across the four depths and is not a
  spike.
- **Synthetic geometry.** Density is uniform over the row space, so every tile at a depth has the
  same length and every shard's part the same fraction; a real corpus's tiles vary in length with
  the map's density, and a real shard's share of a tile with its epoch. The jitter makes the
  boundaries arbitrary but not the lengths.
- **Selection reads one shared column.** Each part reads its own random offset of one 512 MB
  column rather than a column per shard, and depths 0–1 are not measured for select. The
  identities are uniform random `u64`s, as `tessera_id` is by construction.
- **Absolute figures are one thread's.** The sweep's parallel branch divides the wall-clock of a
  large request across the pool; it does not divide the work, and the ratio is what was asked.
- **A 2³⁰-row universe**, not 10⁹, and one machine. The ratios are the finding; the absolute
  microseconds are this box's, single-threaded, with a warm page cache.
