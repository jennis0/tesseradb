# Identity bands: does a band answer a whole-map viewport without the identity column?

Selection needs two quantities per tile (architecture §7.2): `C_θ(T)`, how many of the tile's
visible rows carry a `tessera_id` below the depth's cut `P_d`, and the `m(T)` smallest such
identities. Every shipped route reads the 8 B/row identity column to get them — 28 GB at rung 6,
which does not stay resident under the 24 GiB cap the server runs in, so a whole-map request is a
28 GB disc read. This probe evaluates the same definition a second way, from a band-major
structure built beside the bundle, and asks three questions: does it return the same rows, what
does it read, and can the cut index answer a served row's *position* at cell resolution without
touching `morton.u32` and the residual column.

The proposal is
[`2026-09-14-whole-map-selection-under-a-cap.md`](../../docs/evidence/memos/2026-09-14-whole-map-selection-under-a-cap.md)
§4. It is not accepted and nothing in it is built. Nothing in this probe ships either: the files
below live beside a bundle, no bundle format knows about them, and no request path reads them.

## What it measures

`identity_bands_build` makes one sequential pass over one segment's `tessera_id` and `morton.u32`
and writes, all little-endian:

| file | content | size model |
|---|---|---|
| `lz.u8` | `tessera_id.leading_zeros()` (0..=64), a byte a row | `ceil(3n/8)` |
| `fp16.u16` | `min(lz, 63)` in the top six bits, the ten bits after the leading one in the low ten; `0` maps to `63 << 10` | `2n` |
| `top-J.bin`, J in 4, 6, 8, 10 | `(row: u32, id: u64, code: u32)`, 16 B, every row with `lz >= J`, in row order | `16·n/2^J` |
| `cell-codes.u32` | `morton[cuts[i]]`, one `u32` a cell | `4·cells` |

Sizes and models are both in `bands.json`, with the pass's wall time and its `read_bytes`.
**`lz.u8`'s measured size is 8/3 of its model by construction**: the model prices the three-bit
packed field the structure would ship, and the probe writes a byte a row so a tile's rows can be
addressed without unpacking. The other four measured sizes are the model to within the sampling
of a uniform identity: each `top-J.bin` holds the rows that landed in its band rather than the
expectation, so its ratio is `1 ± O(2^{J/2}/√n)`.

`identity_bands_probe` then runs three arms over one request, per principal and case.

- **R**, the reference: `Engine::viewport`, the shipped path, layers and gather included.
- **B**, the band route, evaluated in the probe against **the engine's own composed mask, cut,
  tile ranges and `SelectParams`** — one accessor, `Engine::composed_mask`, hands over the mask
  the request would be answered from, so the two arms are two evaluations of one input and not
  two transcriptions of the composition rule. **B asserts its served identity set against R's on
  every tile**; a disagreement is recorded with the tile, both sets' sizes and the first differing
  identity, and the run continues.
- **G**, the render: for the rows the case served, `morton.u32[row]` and `residual[row]` read
  scattered, against one `cuts.u32` binary search and one `cell-codes.u32` read for the same rows.

**B is timed in two parts, because it answers two questions.** `search` is steps 1 to 4 — the
band `j = P_d.leading_zeros()`, the candidate rows it offers inside the mask, the exact count and
the served set. `position` is step 5, each served row's cell. The comparison against R is the
probe's own check and is timed as part of neither. The search is further split, by one monotonic
clock read at each of four points a tile, into `candidates` (steps 1 and 2), `count` (step 3) and
`floor` (step 4); the three sum to a little under the search, the difference being the tile loop
itself.

- **The mask walk is timed apart from the work done inside it.** `for_each_visible_run` has two
  routes — with the diffs empty it walks `base` in place with a croaring cursor, otherwise it
  materialises `rows_in_range` for the range first — and either way it builds a fresh cursor and
  seeks to the range per call. `mask_walk_s` is the call time less the time spent inside the
  caller's closure, `mask_walk_inside_s` is that closure's share, `mask_walk_calls` and
  `runs_walked` are the denominators, and `seek_s` / `seeks` are the lists' own binary searches.
  Every run walk the band arm makes goes through this, not only the lists'.
- **The composed mask's shape is recorded per principal**: `diffs_are_empty`, whether a filter is
  set, and the croaring container mix, values and bytes of `base`, `minus` and `plus`. What a run
  step costs is a property of the container it comes out of, and a session granted the whole
  corpus has a differently shaped `base` from one granted a few terms.
- **Membership is a lockstep merge, not a `contains` an entry.** A `top-J.bin` is sorted by row
  and `EffectiveMask::for_each_visible_run` yields the visible rows of a range as ascending runs,
  so one forward cursor settles every entry at the cost of a comparison. Asking the bitmap instead
  is a container search each time, measured at rung 6 at about 700 ns an entry over a
  53,000-container mask, which was the whole of the arm's cost there. The run walk is the decode
  the shipped run tier performs, so the two arms decode the mask the same way; `runs_walked`
  records how many runs it stepped through.

- **The band's own list supplies the position.** A `top-J.bin` entry carries `(row, id, code)`,
  so a served row a list offered needs no lookup at all; only the rest cost a `cuts.u32` binary
  search and a `cell-codes.u32` read. The report counts the two separately.
- **The floor widens through the lists, not through `lz.u8`.** When `C_θ < m`, the route first
  takes the band already in hand — which held fewer than `m` rows *below the cut*, not fewer than
  `m` rows — then steps down the wider lists (`J` below `j`), each a slice of the tile's row range
  located by binary search. A settled step costs no identity read: the entries carry the
  identities. It enters at the list expected to answer rather than at the narrowest: a tile of
  `visible` rows holds about `visible / 2^J` of a list's entries, so the first `J` with
  `visible / 2^J >= 4·m` is the narrowest one likely to reach the floor, and `floor_lists_walked`
  says how many lists were walked in the end. Only where even `J = 4` (one row in sixteen) holds
  fewer than `m` of the tile's visible rows does the route read the tile's visible identities from
  the column, and such a tile has few visible rows. `lz.u8` is read by step 2b alone, the
  sparse-principal route where no list is narrow enough to be the band.
- **The quantised counts.** Beside the exact count, B records the band's own population `|S|` and
  the next band up, both free because every candidate's identity is already in hand. The `fp16`
  count is behind `--fp16` and is **an experiment on top of the route rather than part of it**:
  where a list supplies the candidate its identity comes with it, so the exact count needs no
  quantised prefix and the two-byte column would be read for the comparison alone.

**Cases**, per principal: `whole_k30` (whole extent, zoom 0, k = 30 — the battery's request);
`whole_budget` (whole extent at `d*`, the depth in `0..=9` whose `16 · N_occ(d)` is nearest the
client's mark budget, k = 5000); and, for z in 2, 4, 6, 8, the densest depth-`z` tile requested at
`zoom = min(16, d* + z)` at k = 5000 and at `zoom = z` at k = 30.

**Conditions.** `hot` is the second of two runs. `cold` is `posix_fadvise(POSIX_FADV_DONTNEED)`
over every file under the bundle and the bands directory, then one run — the eviction available
without root on this box, and the one `serve_battery.py` uses.

**Flags that change what is measured**, all off by default except the cell-code check:

| flag | what it does |
|---|---|
| `--arms R,B,G` | which arms run. `B` alone skips the reference and the equality comparison, for a re-run where equality is already established; `G` reads `B`'s served rows and needs it |
| `--cases <names>` | run only these cases |
| `--no-code-check` | skip step 5's assertion that a served row's cell code is its own `morton.u32` entry. The assertion checks the cut index's contract and is not part of the route: it reads the scattered geometry column the route exists to avoid, once a served row |
| `--fp16` | also compute the `fp16` count |
| `--madv-random` | `madvise(MADV_RANDOM)` over the identity column and `lz.u8` before the arms run, so a scattered read stops pulling a read-ahead window |
| `--per-tile <dir>` | write the reference arm's whole per-tile table as NDJSON |

Every figure is measured except these, which are modelled and marked as such in the report: the
4 KiB page counts and the byte figures derived from them in arm G, the `px/cell` bound on the
cell-resolution render's position error, and the size models above.

## What it cannot attribute

- **`posix_fadvise` does not drop a page another process holds mapped**, and the engine holds the
  whole bundle mapped. A cold arm whose major-fault delta is zero proved nothing about being cold.
  Every arm records `majflt` and `read_bytes` beside its wall, so a cold figure that evicted
  nothing is visible as such. `memory.reclaim` on the probe's own scope would reach those pages
  and would also reclaim the probe's own heap, so it is not used.
- **R runs on the engine's pool and B is one thread.** Wall is not comparable between them; CPU is
  (`CLOCK_PROCESS_CPUTIME_ID`, every thread summed — `/proc/self/stat`'s tick figure is beside it
  in the JSON and is 10 ms-resolution on this kernel). `minflt` and `majflt` are both recorded: a
  cold arm's first touch of a mapped page it has already faulted once is a minor fault, so the two
  separate a page the kernel had to fetch from one it merely had to map. R also gathers the response and answers the
  layers; B answers selection alone. B is an upper bound on what the structure costs rather than a
  lower one: it is the route written out, not a tuned version of it.
- **`read_bytes` and `majflt` are the whole process's.** Nothing separates the engine's pool
  threads from the probe's own reads. They are also not the bytes a route *asked* for: a scattered
  read pulls a read-ahead window, so an arm touching a few million rows can move tens of gigabytes
  through the block layer. `--madv-random` is the knob that turns that off for the identity column
  and `lz.u8`; it applies to the *mapping*, so it reaches the reference arm's reads of the same
  column as well as the band arm's, and a served path would decide it per mapping rather than per
  process.
- **The merge's cost is the mask's run count, not the probe's.** Stepping the runs is what the
  shipped run tier does, but a mask whose runs are short in a tile's range costs a step a run
  where a `contains` would have cost one search an entry. On the small corpus the masks are
  country-shaped and hold under one run a tile at every depth measured, so the merge neither helps
  nor hurts there; the mask it was made for is rung 6's.
- **The share of positions a list supplies is a property of `j`.** Where the cut lands in a band a
  list covers, nearly every served row's code arrives with its entry; where it does not, every one
  costs a cut-index search. The two counts are reported separately rather than summed.
- **The zoom-`z` locations are the widest principal's densest tiles**, so that every principal is
  measured at the same places. A sparse principal sees nothing in some of them and its case is
  then empty — six of `p1`'s ten cases are empty on the small corpus. Those rows are real
  measurements of an empty answer, not failures.
- **The band index `j` is a property of the corpus, not of the route.** `j` falls as the mark
  budget rises against the visible row count: at a 2,000,000-mark budget over 25,846,007 rows the
  budget is 8% of the corpus, `j` is 0 to 5, and the four lists (which start at `J = 4`) mostly do
  not apply. The same budget over rung 6's 3,495,729,729 rows is 0.06%, where `j` is around 10.
  **The small corpus therefore measures the equality and not the saving**; the saving is a rung-6
  question.
- **One segment.** The probe refuses a view holding more than one, because §7.2's multi-segment
  rule sums `C_θ` across segments and serves the global bottom-`m`, and a probe that evaluated one
  segment of several would be comparing a fraction of the answer.
- **The tile ceiling is the deployment's, not the type's.** `EngineConfig::max_tiles_per_request`
  is a plain `usize` and validates nothing; the refusal is per request, against the tile count the
  request demands. The probe takes `tessera-server`'s own default, 262,144, at which a
  whole-extent request is served to depth 9 (4⁹ = 262,144 tiles) and refused at depth 10. That is
  why `d*` is searched over `0..=9`.

## How to run

```bash
bash probes/2026-09-14-identity-bands/run.sh [<rung dir>] [<out dir>]
```

The rung defaults to `data/ladder/gbif-64p`; the out directory to `$WORK`, else a directory under
`$TMPDIR`. `run.sh` rebuilds the rung's bundle into the out directory with this tree's own
`tessera build` — a rung's committed bundle may predate the cut index this route reads — then
builds the bands, composes the principal ladder with `serve_battery.py`'s own greedy at 1%, 5%,
10%, 25%, 50% and 100%, runs the probe and prints `report.py`'s Markdown. The builder and the
probe run under `systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G`; `CAP`, `SWAP`,
`BUDGET`, `TARGETS`, `MARK_BUDGET`, `K_SMALL` and `CONDITIONS` override.

Prerequisites: `CARGO_TARGET_DIR=<tree>/target cargo build --release -j 6 -p tessera-cli -p
tessera-bench`, and the rung's `.env` beside its `tessera.toml` (the deployment file `run.sh`
writes names the identity key's *variable*; the value stays in the environment).

**`run.sh` never defaults to `data/ladder/gbif`.** That bundle is 196 GiB and opening it is a
decision. To measure a rung, run the two binaries against it directly:

```bash
# the bands, one pass over the segment, under a cap
systemd-run --user --scope --collect -p MemoryMax=8G -p MemorySwapMax=2G -- \
  nice -n 19 target/release/identity_bands_build \
    --segment <bundle>/v00000/partitions/default/views/geo/segments/seg-0 \
    --out <work>/bands

# the probe, one --principal per rung of the ladder
systemd-run --user --scope --collect -p MemoryMax=24G -p MemorySwapMax=2G -- \
  nice -n 19 target/release/identity_bands_probe \
    --bundle <bundle> --bands <work>/bands \
    --principal p1=<terms> --principal p5=<terms> ... --principal p100=<terms> \
    --budget 2000000 --k-small 30 --conditions cold,hot \
    --out <work>/results.json

# the band arm alone, on a corpus where equality is already established: no reference, no
# comparison, no render, no cell-code assertion, and the two scattered columns advised random
systemd-run --user --scope --collect -p MemoryMax=24G -p MemorySwapMax=2G -- \
  nice -n 19 target/release/identity_bands_probe \
    --bundle <bundle> --bands <work>/bands \
    --principal p1=<terms> ... --principal p100=<terms> \
    --arms B --no-code-check --madv-random \
    --budget 2000000 --k-small 30 --conditions cold,hot \
    --out <work>/results-b.json

python3 probes/2026-09-14-identity-bands/report.py <work>/results.json
```

`--per-tile <dir>` additionally writes the reference arm's whole per-tile table — `(tile, visible,
served)` and the served identities — as NDJSON, one file a principal and case. It is off by
default: a 262,144-tile case is one line a tile, and the equality it would support is checked over
every tile in memory whether or not it is written out.

## Results

**Small corpus, 2026-09-14, `data/ladder/gbif-64p` rebuilt at 25,846,007 rows in one segment over
3,508,005 occupied leaf cells, six principals from 1% to 100%, ten cases each, both conditions,
every arm, the cell-code check on. The box was shared with other sessions throughout, so the wall
figures carry that and the run-to-run spread is a few per cent.**

Every tile of every case agreed between the band route and the shipped selection: 0 disagreements
over the sixty principal-and-case pairs, with the lockstep merge returning the identical candidate
set the per-entry `contains` returned (`Σ|S|` 806,090 and 140,771 identity reads at `p100`'s
`whole_budget`, both unchanged). The builder's file sizes matched their models to within 0.5%
(`top-10.bin`, the smallest list, at 0.9895) except `lz.u8`, whose 8/3 ratio is the packed model
against a byte-a-row file as described above. The band's own population `|S|` ran 1.01× to 2.07×
the exact count and the next band up 0.40× to 0.99×, bracketing it as the powers-of-two spacing
predicts. With `--fp16`, the quantised count equalled the exact count in every case at 2 B a
candidate row plus a handful of identity reads for equal prefixes (379 ties over 588,510 counted
rows in `p100`'s `whole_budget`).

**Where the positions come from decides what step 5 costs.** At `p100`'s `whole_budget` — the
whole extent at the depth a 2,000,000-mark budget chooses — 501,828 of 536,448 served rows took
their cell code from the `top-4.bin` entry that offered them and 34,620 needed a cut-index search,
so the position phase was 7.3 ms cold and 7.6 ms hot against a 120.4 / 99.1 ms arm. Where no list
is narrow enough to be the band the share inverts: `p100`'s `zoom_2` (cut in band 2) took every
one of its 1,134,142 positions from the cut index, at 34.2 / 33.2 ms. Across every case of the run
the split is 793,560 from lists against 6,889,988 from the cut index, because the deep-zoom cases
saturate the threshold and have no band at all.

**Inside the search, the floor is the expensive third.** At `p100`'s `whole_budget` the split is
14.9 ms gathering the band's candidates, 13.2 ms counting and 9.4 ms in the floor, hot, over a
42.4 ms search — down from 29.9 / 16.0 / 27.6 over 80.8 ms before the wholly-visible tiles stopped
going through a cursor. The mask walk itself is 2.33 ms over 48,749 calls, all of them full-range,
where it was 38.1 ms. The floor
widened 24,697 of 36,833 tiles, settling 1,214 on the band already in hand, 3,000 on a wider list
and 20,483 by reading the tile's visible identities from the column — 140,771 reads, which is that
case's whole column traffic, since the list route supplies every other identity. It walked 23,483
lists for 23,483 tiles: one each, because at `j = 5` only `J = 4` is eligible and the entry point
has nothing to choose between. The alternative that was measured first, widening through `lz.u8`,
cost 1.5 MB of that column at the same case and still left the identities to read.

**The wholly-visible gate answers nearly every walk.** 48,749 of `p100`'s 48,749 walks at
`whole_budget` are full-range, 11,574 of `p50`'s 11,917 and 1,668 of `p1`'s 1,956, and the mask's
own cost per call falls from 0.781 µs to 0.048 µs at `p100` accordingly. The runs handed to the
caller are unchanged — 48,749 at `p100`, 2,367 at `p1` — which is what says the gate is a
shortcut and not a different answer.

**Every principal's mask takes the fast decode route, and its `base` is nearly all run
containers.** `diffs_are_empty` is true for all six, `minus` and `plus` are empty and no filter is
set, so no run walk materialises anything. The whole-grant session's `base` is 395 run containers
holding 25,846,007 values in 2,370 bytes — the cheapest shape there is, not a pathological one —
against 50 containers for the 1% principal. The mask's own cost per `for_each_visible_run` call,
with the caller's work removed, runs from 0.166 µs at 36 containers (`p5`) to 0.781 µs at 395
(`p100`), and at `p100` every call yields exactly one run, so that figure is cursor construction
and the seek rather than run iteration.

### What a run walk costs on a bitmap of long runs

**The engine's run decode pays, per call, the containers between the tile's start and the end of
the run the tile sits in.** `EffectiveMask::for_each_visible_run` walks its source through
`for_each_run_in`, which asks the croaring cursor for ranges before it looks at the caller's range
end. The native reader,
[`roaring_uint32_iterator_read_ranges`](https://github.com/RoaringBitmap/CRoaring) — croaring-sys
4.7.1, `CRoaring/roaring.c` line 16868 — merges a run across container boundaries: when a run
reaches a container's end it steps to the next container and keeps merging while that container
begins at `max + 1`. On a bitmap that is one long run, the first range returned from any tile
start is therefore the run to the end of the bitmap, and producing it steps every container in
between. The read buffer's size does not change this: a buffer of four and a buffer of sixty-four
both read that same first range.

Measured at rung 6 (3,495,729,729 rows, whole-grant session, `base` 53,341 run containers of one
full run each, diffs empty, no filter): **84 µs a call** at `whole_budget`, about 27,000 container
steps on the average tile, and **157 µs** at `zoom_2`, whose tile sits earlier in the row space
with more containers ahead of it. The 50% principal, whose runs are short, pays **1.1 µs** a call.
Those three figures are from the rung-6 band-only runs, not from this README's own corpus.

**The shipped path hides this for a whole-grant session and not for every session.**
`select::decode_tier`'s `FullRange` arm answers a wholly visible tile without a cursor, and every
tile of a whole-grant session is of that shape. A session whose mask holds a long contiguous run
that is **not** the whole view — a large country's rows, say — takes the runs tier instead, and
every tile inside that run pays the containers from the tile's start to the run's end. The band
arm applies the same `FullRange` gate (`full_range_walks` counts it) so that the two arms gate a
whole tile the same way. Nothing in the engine was changed; this records what was measured.

**The merge does not show its value here.** The masks on this corpus are country-shaped and hold
under one run a tile at every depth measured (28,266 runs over 36,833 tiles at `p100`'s
`whole_budget`), so a bitmap `contains` is a cheap container hit and the merge replaces it with a
comparable cost. It was made for rung 6, where the same call was measured at about 700 ns over a
53,000-container mask.

What the small corpus does not answer is whether the structure saves anything, for the reason in
"what it cannot attribute": at this row count a 2,000,000-mark budget is 8% of the corpus, so the
whole-map cut lands in band 5 or below and the route reads a large fraction of the segment either
way. `p100`'s `zoom_4` and `zoom_6` cases saturate the threshold outright, and every tile there
falls back to the scan — which is the route's floor behaving as specified, not a defect. The
rung-6 run is the one that answers the design's question.

### Rung 6

The whole GBIF corpus (3,495,729,729 rows, format 11) under the 24 GiB serve cap, 2026-09-14:
every tile of every case agreed, and the band route answers the whole map at a two-million-mark
budget in 0.06 to 0.19 s hot for every principal on the ladder against 0.4 to 81 s for the
shipped path. The tables and the reading of them are in
[`docs/evidence/memos/2026-09-14-identity-bands-measured.md`](../../docs/evidence/memos/2026-09-14-identity-bands-measured.md).
The raw results are in [`rung6/`](rung6/): `results-all-arms.json` (the run with the reference
and render arms, commit `6b0c41e8`), `results-band-only.json` (the final band-only run, commit
`76ff0c18`, readahead off on the identity column), `results-diagnostic.json` (the mask-walk
attribution), the builder's `bands.json`, and `tables.py`, which renders the memo's tables from
them into `tables.md`.
