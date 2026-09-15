# The identity-band route, measured on rung 6

**Date:** 2026-09-14
**Status:** Measured. Every figure is from rung 6 (3,495,729,729 rows, one segment, format 11)
under the 24 GiB serve cap on the WSL2 box, unless marked modelled. The probe, its raw results
and the table script are in `probes/2026-09-14-identity-bands/`.
Nothing here ships: no bundle format knows the files, and the engine is unchanged except for
three `#[doc(hidden)]` accessors the probe reads through. The decisions this asks for are in §8.
The problem it answers is `2026-09-14-whole-map-selection-under-a-cap.md` §1; the proposal it
tests is that memo's §4, as amended below.

## 1. Result

A whole-map viewport at a client budget of two million marks is answered from memory at every
coverage on the principal ladder, with the served identity set equal to the shipped selection's
on every tile. The route reads a band-major structure over the identity column instead of the
column, and the position at cell resolution from the cut index instead of the two position
columns. Single-threaded band route against the shipped path on the engine's twelve-thread pool:

| principal | served | shipped, cold | shipped, hot | shipped read, hot | band route, cold | band route, hot | band route CPU, hot |
|---|---|---|---|---|---|---|---|
| 1% | 43,649 | 10.7 s | 0.40 s | 0 | 9.05 s | 0.062 s | 0.067 s |
| 5% | 58,655 | 20.1 s | 0.51 s | 0 | 1.69 s | 0.068 s | 0.067 s |
| 10% | 279,754 | 28.2 s | 1.75 s | 0.8 GB | 1.84 s | 0.125 s | 0.131 s |
| 25% | 283,072 | 50.8 s | 22.5 s | 32.8 GB | 2.38 s | 0.155 s | 0.169 s |
| 50% | 324,814 | 90.6 s | 52.3 s | 71.4 GB | 1.49 s | 0.155 s | 0.171 s |
| 100% | 1,712,874 | 120.5 s | 81.3 s | 118.3 GB | 1.35 s | 0.194 s | 0.204 s |

The battery's own request, the whole extent at depth 0 with `k = 30`, which took 12 to 15 s for
the dense principals whether hot or cold:

| principal | shipped, cold | shipped, hot | band route, cold | band route, hot |
|---|---|---|---|---|
| 1% | 3.32 s | 0.05 s | 0.35 s | 0.011 s |
| 5% | 5.45 s | 0.23 s | 0.02 s | 0.012 s |
| 10% | 5.63 s | 0.51 s | 0.04 s | 0.028 s |
| 25% | 10.9 s | 6.79 s | 0.04 s | 0.024 s |
| 50% | 14.7 s | 11.3 s | 0.06 s | 0.044 s |
| 100% | 12.9 s | 11.9 s | 0.03 s | 0.017 s |

The shipped path's "hot" figures for the dense principals are disk reads: the identity column is
28 GB and the cap holds about 14 GB of page cache beside the engine's anonymous memory, so the
column is never resident and every request re-reads it. The band route's whole-map working set is
the 55 MB list for band 10 plus the pages of the cut index and cell codes it touches.

## 2. What was measured

Six principals composed by the battery's greedy ladder over `country-ranks.json` (1%, 5%, 10%,
25%, 50%, 100% of rows), each authorised once. Ten cases each: the whole extent at `k = 30`;
the whole extent at depth 9, the depth at which sixteen marks per occupied tile is nearest two
million; and for `z` in 2, 4, 6, 8 the depth-`z` tile with the most visible rows under the 100%
principal, requested at depth `9 + z` (capped at 16) and at depth `z` with `k = 30`. Three arms
per case: the shipped `Engine::viewport` (reference, including gather); the band route over the
same composed mask, cut, tile ranges and selection parameters, taken from the engine through
`Engine::composed_mask`; and the render, positions for the served rows from the two columns and
from the cut index. Two conditions: cold after `posix_fadvise(DONTNEED)` over every file, and
hot as the second of two runs. Counters per arm: wall, process CPU, major and minor faults,
bytes read from the block layer, and the route's own reads.

Equality is asserted per tile on the served identity set in order. Over six principals, ten
cases and both conditions on the small corpus (25,846,007 rows) and on rung 6, no tile
disagreed. The largest comparison was 11,902,722 identities over 243,529 tiles.

The reference figures are from one run of all three arms; the band-route figures are from a
later band-only run after the probe's own overheads were removed (§5). Both runs used the same
bundle, principals, cases and cut.

## 3. The structures

One sequential pass over the identity column and the Morton column, 64 s wall under an 8 GiB
cap, 42.7 GB read, no per-row memory:

| file | content | bytes at rung 6 | model |
|---|---|---|---|
| `top-10.bin` | `(row: u32, id: u64, code: u32)` for rows with 10 or more leading zeros in the identity, row order | 54.6 MB | 16 B × n / 2¹⁰, within 0.1% |
| `top-8.bin` | the same for 8 or more | 218 MB | within 0.1% |
| `top-6.bin` | 6 or more | 874 MB | within 0.1% |
| `top-4.bin` | 4 or more | 3.50 GB | within 0.1% |
| `lz.u8` | leading-zero count per row, one byte | 3.50 GB | 3-bit packed form would be 1.31 GB |
| `cell-codes.u32` | the Morton code of each occupied leaf cell, beside `cuts.u32` | 168 MB | 4 B × cells |
| `fp16.u16` | a 2-byte prefix (6-bit leading-zero count, 10-bit mantissa); §7 | 6.99 GB | 2 B × n |

The leading-zero histogram is the geometric series the model assumes: 1,747,899,968 rows at
zero, halving each step. A cell's code is every one of its rows' code, so `cell-codes` gives a
served row's position exactly at cell resolution; the residual is the only loss.

The route per tile, with cut `P_d` and band `j = P_d.leading_zeros()`:

1. Candidates: the tile's slice of the narrowest list at or below band `j` (located by binary
   search on row), merged in lockstep against the mask's visible runs; entries with fewer than
   `j` leading zeros are dropped. A tile whose visible count equals its row span is wholly
   visible and needs no run walk. Where no list is narrow enough (band below 4) the candidates
   come from the mask's visible rows filtered by `lz.u8`, with the identity read from the column.
2. Count: `C_θ` is the candidates with identity below `P_d`, exact, from the list's own
   identities.
3. Served: if `C_θ ≥ m`, the `m` smallest of those. If the floor binds, widen through the wider
   lists, entering at the first whose expected slice holds four times `m`, and settle on the
   first that holds `m` visible rows; if none does, read the tile's visible identities from the
   column, which at that point is a tile of under about thirty visible rows.
4. Position: the code from the list entry where the row came from one, else one binary search
   in `cuts.u32` and one read of `cell-codes`.

## 4. The shape across zooms

The same budget over the densest tile at each depth, hot. The band index falls two per depth
because the cut widens as the view narrows.

| principal | view | depth | band | served | shipped wall | shipped CPU | band route wall | source |
|---|---|---|---|---|---|---|---|---|
| 100% | whole map | 9 | 10 | 1,712,874 | 81.3 s | 78.0 s | 0.194 s | list |
| 100% | densest depth-2 tile | 11 | 7 | 4,038,307 | 21.0 s | 28.0 s | 0.386 s | list |
| 100% | densest depth-4 tile | 13 | 5 | 11,902,722 | 4.14 s | 7.73 s | 0.697 s | list |
| 100% | densest depth-6 tile | 15 | 3 | 7,974,232 | 1.61 s | 2.75 s | 0.801 s | `lz.u8` and column |
| 100% | densest depth-8 tile | 16 | 2 | 1,912,712 | 0.41 s | 0.60 s | 0.158 s | `lz.u8` and column |
| 50% | whole map | 9 | 12 | 324,814 | 52.3 s | 42.9 s | 0.155 s | list |
| 50% | densest depth-2 tile | 11 | 9 | 1,142,062 | 13.0 s | 17.1 s | 0.118 s | list |
| 50% | densest depth-4 tile | 13 | 6 | 492,742 | 0.43 s | 0.71 s | 0.051 s | list |
| 50% | densest depth-6 tile | 15 | 3 | 1,083,903 | 0.33 s | 0.52 s | 0.111 s | `lz.u8` and column |
| 50% | densest depth-8 tile | 16 | 2 | 3 | 0.02 s | 0.03 s | 0.002 s | `lz.u8` and column |

The shipped path's cost tracks the tiles' row span, not the marks served: the depth-4 view serves
seven times the whole map's points in a twentieth of the time because its span is resident. From
depth 4 inward the shipped path is cache-resident and fast; the band route wins there too on
CPU, but the margin that matters is at the two outer rows, where the shipped path reads the
column from disk on every request.

A served count larger than the budget at the deeper views is the definition working: the
threshold is a per-occupied-tile target over the whole view, and a dense region holds more
occupied tiles than average. A client holding to a budget in view chooses a shallower depth
there.

The gate between the routes is the band index, a function of the cut alone and so identical for
every principal. Lists exist down to band 4; below band 6 the shipped route's span is small
enough to be resident and the `lz.u8` route offers no saving over it.

## 5. Where the route's time goes

The search split at the whole-map budget, hot, single-threaded:

| principal | tiles | candidates | list bytes | identity reads | floor-bound tiles | settled by band / list / column | candidates / count / floor | position |
|---|---|---|---|---|---|---|---|---|
| 1% | 2,942 | 68,277 | 371 MB | 7,292 | 2,335 | 38 / 535 / 1,762 | 10 / 1 / 44 ms | 1.2 ms |
| 10% | 27,217 | 683,334 | 320 MB | 107,225 | 23,137 | 741 / 8,999 / 13,397 | 39 / 12 / 56 ms | 3.5 ms |
| 50% | 23,070 | 427,162 | 466 MB | 57,195 | 18,660 | 160 / 7,217 / 11,283 | 51 / 8 / 75 ms | 4.6 ms |
| 100% | 154,835 | 3,411,766 | 69 MB | 662,564 | 132,024 | 2,544 / 65,954 / 63,526 | 31 / 59 / 70 ms | 9.3 ms |

Three findings from getting to these figures:

- **The floor is the largest term and is inherent to I7 at whole-map depth.** For the 100%
  principal 132,024 of 154,835 occupied tiles hold fewer than two admitted identities and take
  the floor; 63,526 of those hold fewer than about thirty visible rows and are read from the
  column, 662,564 identities in all. That is the whole of the route's column traffic. Under the
  cap those are scattered pages; with `MADV_RANDOM` on the identity column the cold whole map
  reads 0.65 GB for the 100% principal. The cold figure for the 1% principal (9 s, 3.1 GB) is
  readahead over `top-4.bin` slices for its floor tiles; that list is 3.5 GB and is the one to
  drop or pin.
- **Per-entry mask lookups cost 700 ns on a 53,000-container bitmap.** Merging the list slice
  against the mask's runs in lockstep replaced them; the candidate set is identical.
- **The run reader merges across container boundaries.** croaring's
  `roaring_uint32_iterator_read_ranges` (croaring-sys 4.7.1, `CRoaring/roaring.c` line 16868)
  continues a run into the next container while values stay consecutive, so on a whole-grant
  bitmap the first range read from any tile is the run to the end of the bitmap, 84 µs a tile at
  the whole map. The shipped full-range tier hides this for a session whose mask covers the
  view; the probe applies the same check. A session whose mask holds a long contiguous run
  that is not the whole view takes the runs tier in the shipped engine and pays the containers
  from the tile's start to the run's end on every tile inside it. Not fixed; the engine is
  unchanged.

The reference arm's own reads say what the shipped render costs at this scale: the hot whole map
for the 100% principal moved 118 GB for a 28 GB column and 5.3 GB of modelled position pages,
because readahead over scattered faults multiplies the bytes about four times.

## 6. The render

Positions for the served rows of the whole-map case, from the two position columns against the
cut index with cell codes:

| principal | rows | columns, cold | columns, hot | column pages, modelled | cut index, cold | cut index, hot | cut index pages, modelled |
|---|---|---|---|---|---|---|---|
| 25% | 283,072 | 2.17 s | 6.41 s | 1.10 GB | 0.076 s | 0.060 s | 0.128 GB |
| 50% | 324,814 | 12.5 s | 10.7 s | 1.33 GB | 0.124 s | 0.038 s | 0.183 GB |
| 100% | 1,712,874 | 17.7 s | 19.0 s | 5.28 GB | 0.207 s | 0.139 s | 0.335 GB |

The two columns never become resident under the cap for the dense principals. The position
error of the cut-index form is the residual alone: under one leaf cell, 2⁻¹⁶ of the extent per
axis, 0.03 px in a 2,000 px whole-map view and one pixel at zoom 8. Where the list supplies the
row, the code comes with it and the cut index is not consulted (1,596,319 of 1,712,874 rows for
the 100% principal).

## 7. Exactness

The count is exact from the lists' identities; nothing above depends on quantising the cut. Two
alternatives were measured beside it. Rounding the cut down to a band boundary would serve 0.58
to 0.98 of the exact marks per case; rounding up, 1.16 to 1.57. Octave bands are too coarse for
that. The 2-byte prefix column (`fp16.u16`) gave the exact count in every case with one identity
read per 1,100 to 1,500 counted rows to settle ties; it would matter only for a route without the
lists' identities, and none of the routes above is one.

## 8. What it does not fix, and the decisions

Not fixed here: session start, which at rung 6 is a 6 to 38 s projection build per principal;
the handover for it is `2026-09-14-session-start-handover.md`. Nor the emit. The serve battery
at the client's request shape (`test_corpora/common/serve_battery.py`: the whole extent at depth
9, `k` at the ceiling, layers on; `rung6/battery-budget.json` beside the probe's results) measured the
end-to-end whole map hot at 0.6 s for the 1% principal, 23 s at 10%, 35 s at 25%, 80 s at 50%
and 95 s at 100%, of which the time to first flush, which is what the search above replaces, was
0.1, 6.8, 5.5, 11.5 and 13.6 s. The remainder is the emit: the scattered gather of the served
rows' positions and scalars, which the cell codes address for the position, and the artifact
channel, which at depth 9 with every level of the taxonomy layer in view is 16 s and 51 MB for
the 10% principal and about 200 MB of the 100% principal's 238 MB body. A default deployment
sheds that body: its 60 s whole-stream deadline cut the 100% principal's whole map mid-stream
until the deadline was raised for the measurement. The artifact channel at whole-map depth is
the next read-path problem after this one. The floor's column pages under the cap remain.

For the owner:

- **A. Adopt the route, and which files.** The lists from band 6 up (1.15 GB at rung 6) serve
  the whole map and the next two depths for every principal; `top-4.bin` (3.5 GB) serves band
  4 and 5 only, where the shipped route is resident, and its readahead is the 1% principal's
  cold cost. `lz.u8` is read only below band 6 and only by the probe's own route there. The
  recommendation is the lists from band 6 up and `cell-codes.u32`, with `lz.u8` and the
  wider list dropped, the gate at band 6, and the shipped route below it. Every file is a pure
  function of the identity column, written by the one segment writer and recomputed by
  `verify --deep`; format 11 to 12.
- **B. Exactness.** Keep the exact cut; the lists carry full identities and cost nothing for it.
  No change to §7.2's definition.
- **C. Positions at cell resolution for shallow requests.** A request at depth 8 or shallower
  can carry codes with a zero residual and a flag; the client draws them sub-pixel. A contract
  change to §3.2.
- **D. The run reader.** Whether the engine's `for_each_run_in` should bound its read for
  long-run masks that are not the whole view.

## 9. Pointers

- `probes/2026-09-14-identity-bands/`: README (method, what it
  cannot attribute, the run reader finding), `run.sh`, `report.py`, and `rung6/` with the raw
  results of the three rung 6 runs, the builder's `bands.json`, and `tables.py`, which
  renders every table above from them.
- `crates/tessera-bench/src/bin/identity_bands_build.rs` and `identity_bands_probe.rs`.
- The problem and the proposal: `2026-09-14-whole-map-selection-under-a-cap.md`; the campaign
  record: `../../ingest-campaign.md` §4d; the selection definition: architecture §7.2 and
  `crates/tessera-engine/src/select.rs`.
