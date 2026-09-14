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
  two transcriptions of the composition rule. Per tile: the band `j = P_d.leading_zeros()`, the
  candidate rows the band offers inside the mask, the exact count, the served identities, and the
  cell of each served row from `cuts.u32` and `cell-codes.u32`. **B asserts its served identity
  set against R's on every tile**; a disagreement is recorded with the tile, both sets' sizes and
  the first differing identity, and the run continues.
- **G**, the render: for the rows the case served, `morton.u32[row]` and `residual[row]` read
  scattered, against one `cuts.u32` binary search and one `cell-codes.u32` read for the same rows.

Beside the exact count, B records three quantised counts over the tiles a band settled: `|S|` (the
band's own population inside the mask), the next band up, and the `fp16` count — which compares
quantised prefixes and reads the identity column only where two prefixes are equal.

**Cases**, per principal: `whole_k30` (whole extent, zoom 0, k = 30 — the battery's request);
`whole_budget` (whole extent at `d*`, the depth in `0..=9` whose `16 · N_occ(d)` is nearest the
client's mark budget, k = 5000); and, for z in 2, 4, 6, 8, the densest depth-`z` tile requested at
`zoom = min(16, d* + z)` at k = 5000 and at `zoom = z` at k = 30.

**Conditions.** `hot` is the second of two runs. `cold` is `posix_fadvise(POSIX_FADV_DONTNEED)`
over every file under the bundle and the bands directory, then one run — the eviction available
without root on this box, and the one `serve_battery.py` uses.

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
  in the JSON and is 10 ms-resolution on this kernel). R also gathers the response and answers the
  layers; B answers selection alone. B is an upper bound on what the structure costs rather than a
  lower one: it is the route written out, not a tuned version of it.
- **`read_bytes` and `majflt` are the whole process's.** Nothing separates the engine's pool
  threads from the probe's own reads.
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

python3 probes/2026-09-14-identity-bands/report.py <work>/results.json
```

`--per-tile <dir>` additionally writes the reference arm's whole per-tile table — `(tile, visible,
served)` and the served identities — as NDJSON, one file a principal and case. It is off by
default: a 262,144-tile case is one line a tile, and the equality it would support is checked over
every tile in memory whether or not it is written out.

## Results

**Small corpus, 2026-09-14, `data/ladder/gbif-64p` rebuilt at 25,846,007 rows in one segment over
3,508,005 occupied leaf cells, six principals from 1% to 100%, both conditions.**

Every tile of every case agreed between the band route and the shipped selection: 0 disagreements
over the sixty principal-and-case pairs. The builder's file sizes matched their models to within
0.5% (`top-10.bin`, the smallest list, at 0.9895) except `lz.u8`, whose 8/3 ratio is the packed
model against a byte-a-row file as described above. The `fp16` count equalled the exact count in
every case, at 2 B a candidate row plus a handful of identity reads for equal prefixes: 379 ties
over 588,510 counted rows in `p100`'s `whole_budget`, 1,188 over 1,320,729 in its `zoom_2`. The
band's own population `|S|` ran 1.01× to 2.07× the exact count and the next band up 0.40× to
0.99×, bracketing it as the powers-of-two spacing predicts.

What the small corpus does not answer is whether the structure saves anything, for the reason in
"what it cannot attribute": at this row count a 2,000,000-mark budget is 8% of the corpus, so the
whole-map cut lands in band 5 or below and the route reads a large fraction of the segment either
way. `p100`'s `zoom_4` and `zoom_6` cases saturate the threshold outright, and every tile there
falls back to the scan — which is the route's floor behaving as specified, not a defect. The
rung-6 run is the one that answers the design's question.
