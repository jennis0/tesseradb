# Whole-map selection under a memory cap: the problem and a proposal

**Date:** 2026-09-14
**Status:** Provisional. A handover for whoever picks this up. §1 to §3 are measured on rung 6
and stand. §4 is a proposal the owner has not accepted; nothing in it is built. §6 is the
experiment to run before any design is written. The campaign record for the builds and batteries
cited here is [`../../ingest-campaign.md`](../../ingest-campaign.md) §4d.

## 1. The problem, measured

Rung 6 is the whole GBIF corpus: 3,495,729,729 rows in one view, one segment, one layer of three
levels, a 196 GiB bundle at format 11. The serve box is WSL2 with 47 GiB, and the server runs
inside a cgroup with `MemoryMax=24G` and `MemorySwapMax=0`. Under that cap a whole-map viewport
(zoom 0, the whole extent, layers on, `k = 30`) for a principal who sees the whole corpus takes
15 to 17 s hot. A principal who sees 50% takes 10 to 13 s. Principals who see 1%, 10% and 25%
take 57 ms, 453 ms and 1.06 s. Zoom 6 and 12 answer in 1 to 100 ms for every principal. Cold
requests at every zoom take 5 to 51 s.

The sampler over those requests shows the server reading 2 to 3.3 GB/s from disk with 8.7 GB of
file pages resident beside 15.7 GB of anonymous memory. The time is the identity column. Selection
at a tile needs two quantities from the visible rows of the tile, both defined in architecture
§7.2:

```
C_θ(T) = |{ i ∈ vis(T) : tessera_id(i) < P_d }|
m(T)   = min(cap, max(min(k_min, cap), C_θ(T)))
```

and the response carries the `m(T)` visible rows of lowest `tessera_id`. The cut is per session
and per depth: `P_d = ⌊m_target · N_occ(d) · 2⁶⁴ / V_total⌋`, where `V_total` is the session's
visible row count and `N_occ(d)` the occupied tiles at depth `d`. To evaluate either quantity the
engine reads identities from `columns.arrow`, whose `tessera_id` column is 8 bytes a row, 28 GB at
this rung. Rows are stored in `(morton, tessera_id)` order, so within a leaf Morton cell
identities ascend (contracts §2.6 r6).

Three routes exist and all three touch the whole column at this scale:

| route | when it runs | what it reads | why every page is touched |
|---|---|---|---|
| per-row scan (`Values` tier) | the mask is sparse | every visible row's identity | visible rows are scattered over the column |
| per-cell (`FullRange` and `Runs` tiers, built 2026-09-14) | the mask is dense and the segment has at least six rows a cell | one binary search per occupied cell, plus the cell's head for the sample | a cell at rung 6 averages 83 rows, 664 bytes; a 4 KiB page holds about six cells; 41,899,178 cells touch all 7 million pages |
| per-cell with the scan's fallback | the segment has under six rows a cell | every row | as the scan |

Resident, the per-cell route costs about 0.2 s at rung 6 against 2.8 s for the scan (modelled
from measured per-cell and per-row rates, `crates/tessera-engine/examples/cell_route.rs`). Under
the cap neither is resident, and both cost the 28 GB read.

Two further facts change what a fix must achieve:

- **The battery's request is not a client's.** `serve_battery.py` asks for `k = 30` a viewport.
  A client asks for a drawn-mark budget of 1 to 2 million points a viewport (owner, 2026-09-14),
  which the request-time budget spreads over the viewport's tiles (§7.2, "what bounds a response
  is a request-time budget"). The cut then admits 1 to 2 million identities, and the search and
  the render both scale with that. Every figure above was measured at `k = 30`; a whole-map
  request at the real budget has not been measured at rung 6.
- **The render reads geometry for every sampled row.** A sampled row's position is read from
  `morton.u32` and the residual column (`viewport.rs`, the deinterleave around line 2053).
  Two million scattered reads over a 14 GB Morton column touch about 40% of its pages. That cost
  is paid whatever the search does.

The allocator makes the cap tighter than it looks. After a six-principal battery the server holds
14.15 GiB of freed memory in 373 glibc arenas, because nothing on the serve path calls
`malloc_trim` (measured 2026-09-14; the build path trims). That memory evicts the bundle's pages:
`memory.events max` 19,915,452, 1.2×10⁹ file refaults, 8 TB read in 104 minutes. A branch adding
the trim, an arena cap and the missing figures on `/control/status` is in progress
(`serve/allocator-retention`). With it the cap leaves about 22 GB of page cache. The identity
column does not fit in that either.

## 2. What is in place

All merged on main by 31af45f2, refereed, measured on the 25,846,007-row prefix
`data/ladder/gbif-64p` and on rung 6:

- **A per-segment cut index**, `cuts.u32`, the row start of each occupied leaf Morton cell,
  written by the one segment writer that build, flush, merge and fold share, mapped at open,
  checked by `verify --deep`. 167.6 MB at rung 6. Bundle format 11.
- **Selection per cell on the dense tiers**, gated on six rows a cell (`select.rs`,
  `CELL_ROUTE_MIN_ROWS_PER_CELL`), returning rows identical to the scan. Appendix C row C19
  names its early exit and the gate.
- **Candidacy from the cached histogram** for a row-major level when the viewport covers the
  mask, which removed a second per-row scan (5.4 ns a visible row) from every whole-map request
  with layers.
- **The session projection built in windows**, with a whole-grant short-circuit and run
  optimisation, which removed a 4 B a visible row transient (14 GB at rung 6; it OOM-killed the
  capped server once).

What these did not remove is the identity read itself, and the sparse tier's linear cost.

## 3. Constraints on any fix

- **Identifiers stay 64-bit.** A 32-bit `tessera_id` would halve the column to 14 GB and fit
  under the cap. The owner declined it (2026-09-14): sharding and sharing need the width.
- **I7 and decision 0008.** Sampling happens after masking, by direct evaluation from the
  composed mask. A fixed-width candidate list per tile was declined because a sparse principal's
  tiles go blank. Any structure the selection reads must address rows, and the answer must be the
  same function of the mask that the scan computes. The `k_min` floor is the I7 guarantee and may
  not be removed.
- **Appendix C row C19.** Work that varies with where the principal's own identities fall is an
  accepted widening of C4's shape under C14's reasoning; a new route of that shape is named in
  that row, and a gate must read only what the principal already has or what is mask-free.
- **One implementation** (decisions 0091 and 0139). A structure written at build is written by
  flush, merge and fold through the same writer, and read back by `verify --deep`. Every stale
  bundle is refused by a format bump (decision 0048); no compatibility shim.
- **No row-sized anonymous memory at build** beyond what the residency model names (the
  bounded-assembly design, `2026-09-12-bounded-assembly-design.md`). A whole-row bitmap at n/8
  bytes is charged as a term, not forbidden.
- **The multi-segment rule** (§7.2): sum `C_θ` across segments and serve the global bottom-`m`
  of the union; each segment offers its own bottom-`cap`.

## 4. Proposal: priority bands

Not accepted. Not built.

### 4.1 The structure

`priority` is a prefix of `tessera_id` (§7.2, and the storage section at architecture line 615).
Per segment, store a ladder of bitmaps over rows:

```
B_j = { row : tessera_id(row) < 2⁶⁴ / 2^j }     for j = 1 … J
```

`B_1` holds the half of the rows with the smallest identities, `B_2` a quarter, and so on;
`B_{j+1} ⊂ B_j`. Each is a Roaring bitmap in row order, mask-free, mapped at open like the cut
index. For the bands small enough that a client budget lands in them, also store the band's
`(row, tessera_id)` pairs contiguously, sorted by row. At rung 6 (modelled from the row count):

| j | rows in `B_j` | bitmap form | bitmap bytes | pairs bytes |
|---|---|---|---|---|
| 1 | 1.75×10⁹ | bitset | 437 MB | none |
| 4 | 2.2×10⁸ | bitset | 437 MB | none |
| 8 | 1.4×10⁷ | array | 27 MB | 164 MB |
| 10 | 3.4×10⁶ | array | 7 MB | 41 MB |
| 12 | 8.5×10⁵ | array | 2 MB | 10 MB |
| 16 | 5.3×10⁴ | array | 0.1 MB | 0.6 MB |

Dense bands are bitsets at n/8 bytes each; from about `j = 6` the array form halves each step.
The whole ladder is about 2 GB a segment at rung 6, and the pair lists for every band under a
few million rows total under 300 MB. `J` is a constant of the format, about 40; a corpus smaller
than `2^j` rows has empty bands above `j`.

Where pairs are stored is a threshold in rows, a constant of the format, to be set by the
experiment in §6 from the budget a client sends.

### 4.2 Evaluating a tile

For a tile `T` at depth `d` with cut `P_d`, pick the smallest band containing the cut:
`j = ⌊log₂(2⁶⁴ / P_d)⌋`, so `B_j ⊇ {row : id < P_d}` and `|B_j| < 2 · |{id < P_d}|` in
expectation over a blinded identity.

1. `S = mask ∩ B_j ∩ rows(T)`. A tile at any depth is one contiguous row range, so this is one
   range intersection per band consulted, costed by containers touched. No identity is read.
2. Read the identity of each row in `S`: from the band's pair list if it has one (a sequential
   scan of the list over the tile's row range, one mask lookup a pair), otherwise from the
   column (one scattered read a row). Keep the rows with `id < P_d`. Their count is `C_θ(T)`
   exactly, because every row outside `B_j` has an identity at or above `2⁶⁴/2^j > P_d`.
3. `m(T)` follows from `C_θ(T)` as today. If `C_θ(T) ≥ m(T)`, the bottom-`m` identities of the
   tile are among the rows kept in step 2, and a partial sort of them is the sample. If
   `C_θ(T) < m(T)`, the floor applies: step to `B_{j-1}`, repeat, and stop when the band holds
   at least `m(T)` visible rows or `j = 0` (the whole tile), which is the walk the scan does
   today and serves `k_min` as I7 requires.

Every quantity is evaluated from the composed mask over rows the band addresses. The band is a
partition of the identity space, the same for every principal, and its sizes are a function of
the identity permutation. A sparse principal is not short-changed: a band that holds fewer than
`m` visible rows widens until it does, and the tile walk is the last band. This is the sentence
that belongs in §7.2 and in Appendix C if the proposal is accepted.

### 4.3 Cost at rung 6, modelled

For a whole-map request at a 2 million point budget across the viewport's tiles, the cut admits
about 2 million identities in total, so the band consulted has about 4 million rows and a pair
list of about 48 MB. The search is then one range intersection per tile and one pass over the
pair list, about tens of milliseconds, with no page of the identity column touched. At `k = 30`
the same request touches a band of a few hundred rows. Cold and hot converge, because the band
files are small enough to stay resident under any cap that holds the projections.

The render is separate. At any zoom up to 16 a sampled row's cell is its position at the
resolution the client draws, and the cut index gives the cell of any row by one binary search
over 168 MB resident. Not built: the render reads `morton.u32` and the residual column for each
sampled row. Serving cell-resolution positions at zoom ≤ 16 from the cut index would remove
about 10 GB of scattered geometry reads from a 2 million point whole-map request, and is a
change to the render path, not the search. It should be measured with the search change, not
assumed to follow from it.

What the proposal does not fix: the first viewport's projection walk through `permutation.bin`
(one sequential pass at a dense grant, 14 GB read once a session); the allocator retention
(§1, in progress); and the histogram walk for a row-major level once a session.

### 4.4 Build, ingest, verify

At segment write, one pass over the identities in row order appends each row to `B_1 … B_lz`
where `lz` is the count of leading zero bits of its identity, about two appends a row in
expectation. The dense bands are appended in row order, so a streaming Roaring writer produces
them without holding a bitset a band; the sparse bands and the pair lists are small. The
residency model charges what the build holds. The same writer serves build, flush, merge and
fold. `verify --deep` recomputes each band from the column and compares, as it does for the cut
index. Format 11 → 12.

### 4.5 What the design documents would change

Architecture §7.2 gains the band route beside the three tiers and the per-cell route, with the
gate that chooses between them. Appendix C row C19 names the band route as work keyed on the
principal's own identities in the same accepted shape. Contracts §2.6 and §0.2 name the files.
The storage tree lists them. `select.rs`'s module doc carries the cost table. None of this is
written.

### 4.6 Open questions the proposal does not settle

- **Band spacing.** Powers of two give a band at most twice the cut. Finer spacing halves the
  pair list scanned at the cost of more bands; the experiment should say whether it matters.
- **The pair-list threshold.** Which bands carry pairs is set by the largest budget a client
  sends. Above it the route falls back to scattered identity reads, which is still bounded by
  the band and far below the column.
- **Rows above the base.** A flushed segment's rows lie above `base_rows` and the tile's row
  range spans segments; bands are per segment, and the multi-segment rule applies unchanged.
  The delta tiers (rows written since the last fold) need the same structure written at flush.
- **Suppressed rows.** They are in the projection and removed by `minus` at composition, so the
  mask already excludes them before step 1. Nothing in the band route reads them.
- **The `Values` tier.** The band route is density-independent, so the tier gate's purpose
  changes: the tiers chose how to decode the mask, and the band route reads the mask only
  through `and_cardinality` and `contains`. Whether the tiers survive as the mask-decoding
  choice inside step 1, or the route replaces them, is a design question.
- **Exactness proof.** Step 2's claim rests on `B_j ⊇ {id < P_d}` and on the pair list holding
  every row of `B_j`. Both are properties `verify --deep` can check. The equivalence test should
  keep the scan as the reference, as the per-cell test does.

## 5. Alternatives considered

- **32-bit identifiers.** Declined by the owner (§3).
- **Per-cell heads**, the first `m` identities of each cell stored apart from the column, 1.3 to
  2.7 GB at rung 6. Answers the count only where the cut lies within the head and needs a
  hierarchical tree over the Morton nesting to avoid stepping every cell; dense tiers only. The
  band route subsumes it.
- **A larger cap or box.** A 64 GB box holds the column and the per-cell route pays its 0.2 s.
  The whole-map request stays a 28 GB working set, and the render's 10 GB stays scattered.
- **Accept the figure.** Record that whole-map zoom under a cap this tight is disk-bound at this
  row count and measure elsewhere. This leaves the sparse tier's linear cost and the render cost
  in place.

## 6. The experiment to run first

The bands are a pure function of the identity column, which the current bundle has. Nothing
needs rebuilding.

1. **Stop the running rung 6 server** by pid (it is still up from the battery; the trim branch
   needs a fresh serve anyway). Run everything below inside
   `systemd-run --user --scope --collect -p MemoryMax=… -p MemorySwapMax=0`. Never open the
   bundle uncapped.
2. **Build the bands offline** into the session scratchpad, not into the bundle: one
   sequential pass over the `tessera_id` column of
   `data/ladder/gbif/bundle/v00000/partitions/default/views/geo/segments/seg-0/columns.arrow`,
   writing `B_1 … B_40` as portable Roaring files and the pair lists for bands under 8 million
   rows. Record each band's row count and bytes; compare with the §4.1 model.
3. **A probe binary** on a throwaway branch (a worktree with its own target), opening the bundle
   through the engine so it has real composed masks: authorise the battery's six principals from
   `data/ladder/gbif/country-ranks.json` and `.env`. For each principal and each of: the whole
   extent at the depth a 2 million budget selects, the whole extent at `k = 30`, and the
   battery's decile-9 cells at zoom 6 and 12: compute `C_θ` and the bottom-`m` by the band route
   and by `Selection::of`, assert equality, and record wall, server CPU, bytes read and major
   faults from `/proc/self/io` and `/proc/self/stat` for each, with the page cache dropped
   between conditions where a cold figure is wanted. Report the identity reads the band route
   made per request.
4. **Measure the render separately**: for the sampled rows of the 2 million case, the bytes and
   pages read from `morton.u32` and the residual column today, and the cost of a cell lookup in
   the cut index for the same rows.
5. **Exit criteria.** The band route's answers equal the scan's on every case. Its identity
   reads a request are within 2× the modelled band size. Its wall at the 2 million budget under
   the cap is below one second for the search alone. If any of these fails, the report says
   which and why before any design is written.

## 7. Pointers

- Code: `crates/tessera-engine/src/select.rs` (selection, the tiers, the per-cell route, the
  gate, the cost table); `crates/tessera-store/src/read.rs` (`CutIndex`); the segment writer in
  `crates/tessera-store/src/write.rs`; the build's assembly in
  `crates/tessera-build/src/assembly.rs`; the render's position read in
  `crates/tessera-engine/src/viewport.rs` near the deinterleave; `examples/cell_route.rs`.
- Design: architecture §7.2, §5.2, Appendix C (C4, C14, C19); contracts §2.6; decisions 0008,
  0014, 0048, 0091, 0139.
- Measurements: `ingest-campaign.md` §4d (three builds, two batteries); the session scratchpad
  `rung6/` (build logs, `battery-capped.json`, 10 s samples `serve-proc.tsv`); the memory
  investigation's figures are in §4d Finding D and the trim branch's report.
- The bundle: `data/ladder/gbif/bundle`, format 11, built 2026-09-14 on 488e43e5, verified.
  41,899,178 occupied leaf cells, 83.4 rows a cell, measured from `morton.u32`.
- Rulings taken 2026-09-14: 64-bit identifiers stay; C19 amended; the rows-per-cell gate at six.
  Ruling open: this proposal; the caching design's sizing rule (`caching.md` §4 sums
  projections and omits the page cache and the allocator, which bind at this scale).
