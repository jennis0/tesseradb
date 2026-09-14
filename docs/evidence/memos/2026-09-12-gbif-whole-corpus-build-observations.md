# What the whole-corpus GBIF build showed, and what to pick up after it

**Date:** 2026-09-12
**Status:** Provisional. A running record kept during the build of rung 6, 3,495,729,729 placed
occurrences from `data/ladder/gbif`, on main at f922b294 with `--memory-budget 24g` and
`--stage-timings`. Each item names what was measured, the proposed cause, and the fix proposed
so far. Nothing here is decided. The build's own figures go to
[`../../ingest-campaign.md`](../../ingest-campaign.md) when it finishes.

**Box.** WSL2, 12 cores, 48 GiB in the VM, 12 GiB swap, local NVMe-backed VHDX, 440 GB free at
the start of the run after `target/` was deleted. Sampled every 10 s from `/proc/<pid>/stat`,
`/proc/<pid>/io`, `/proc/<pid>/status` and `/proc/vmstat`.

**Reference.** The 25,846,007-row spread fraction's stage times scaled by row ratio, 135.3×.
Linear is the null hypothesis; an item below is a stage that departed from it.

| stage | fraction | linear at 3.5×10⁹ | measured |
|---|---|---|---|
| `source_ids` | 1.0 s | 135 s | 109 s |
| `dictionary` | 5.2 s | 704 s | 777 s |
| `geometry_read` | 5.5 s | 744 s | 757 s |
| `signature_sort` + `assignment`, ten batches | 6.6 s | 893 s | see §1 |
| `attribute_tail` | 8.2 s | 1,110 s | |
| `layers` | 20.8 s | 2,815 s | |
| `filter_postings` | 5.3 s | 717 s | |
| `record_blob` | 5.2 s | 704 s | |
| `tiler_sort` + `segment_write` | 2.2 s | 298 s | |
| `manifests` | 12.1 s | 1,637 s | |

## 1. The assignment walk writes each entity-map page many times over

**Measured.** The ten `signature_sort` stages took 20 to 29 s each. The ten `assignment` stages
took 110, 65, 65, 152, 187, 204, 233, 219 s and rising, over identical batches of 369,098,752
items. During an assignment the process runs one core at 100%, reads nothing, and issues 200 to
380 MB/s of writes while the bundle root grows at about 20 MB/s. The minor-fault rate tracks the
write rate at one 4 KiB page a fault: 50,000 to 90,000 faults a second.

**Cause.** The walk visits records in signature order and writes each one's entity id into the
mapped `entity-of-ordinal.u32` at its ordinal, so writes within a batch are scattered over a 1.5
GB slice of a shared file mapping. The kernel writes a dirty page back, write-protects it, and the
next scattered write to it faults and dirties it again. A page in the slice is written to disk
many times before the walk leaves it. The cost rises across batches as the dirty set grows and
writeback runs more often.

**Measured, whole loop.** Ten sorts 204 s, ten assignments 1,457 s, 1,672 s against 893 s
linear. The assignment cost levelled at about 200 s a full batch from batch six; the tenth batch,
the 173,840,961-row remainder, took 46 s.

**Proposed fix, in two steps.** First, remove the writes: a batch's slice of the entity map is
the contiguous ordinal range `[ordinal_lo, ordinal_hi)`, so fill a heap `Vec<u32>` of `batch_len`
in the walk and write it to the file once, sequentially, at the end of the batch. This adds 4 B a
batch item to `plan_build`'s per-batch term and should return the walk to its 65 s floor. Second,
and only after measuring the first: the walk has no serial dependency. The entity-map write and
the agreement check are independent per record; the band pairs and the transpose need ascending
entity order, and entity is position in the batch, so chunking the batch by position with one
output buffer per chunk, concatenated in order, keeps it. The 65 s floor is three random accesses a
record over a 1.5 GB slice, a 1.5 GB mapped tally and a 3 GB pairs array, memory-latency bound on
one core, so chunking scales until memory bandwidth binds.

## 2. `appearances` is 4 B an item of anonymous memory the plan does not name

**Measured.** Anonymous RSS sits at 24.3 GB through the batch loop, the budget itself. The
per-batch structures (packed pairs, sort records, starts) are 24 B an item of the batch, as the
plan charges. The rest is `appearances`, `vec![0u32; n]` at 13.3 GB, held from the geometry read
to the end of the loop.

**Cause.** `plan_build`'s fixed term still charges 4 B an item for the label-agreement tally,
kept after the tally became a file so the batch stride and the entity-id assignment do not change
(disk-use ruling 6, 2026-09-10). That retained term covers `appearances` by coincidence, so the
plan reads right and the loop does not swap.

**Proposed fix.** Name it. Either `appearances` becomes a mapped file beside the tally, or the
fixed term's comment says which array the 4 B now stands for. If the tally term is ever removed to
buy larger batches, `appearances` goes to a file in the same change or the loop runs 13 GB over
plan.

## 3. A stale binary held two 13 GB heap arrays and a 27 GB source-id file

**Measured.** A first attempt on the binary from 48c96da3 held 41 GB anonymous plus 9 GB swapped
in the batch loop: `distinct_of_ordinal` and `appearances` at 13.3 GB each on the heap, and a
26.7 GB `source-ids.u64` mapped. Its second batch's assignment took 914 s and its third 2,966 s,
swap-bound. Killed at batch four of five after 2 h 45 m.

**Cause.** `cargo build --release -p tessera` fails because no package is named `tessera`, and
the pipeline's exit code was read from `tail`. The binary that ran predated the tally-as-file and
range-route commits.

**Proposed fix.** A build script that runs the campaign checks the binary's commit against
`HEAD` before starting, or `tessera --version` prints the commit and the log records it.

## 4. `attribute_tail` rewrites the value columns' pages the same way

**Measured.** 1,471 s against 1,110 s linear, 1.33×. Over the stage the process read 47 GB, the
points file once, and wrote 123 GB while the bundle root grew 34 GB. Two cores busy; 40 to 160
MB/s of writes throughout. Anonymous memory 1.8 GB.

**Cause.** The pass reads the points file in ordinal order and writes each value at its entity
index into the mapped column files: `column-0.col` (`kingdom`, 1 B a row, 3.5 GB), `column-2.col`
(`year`, 2 B a row, 7.0 GB) and three presence bitmaps of 437 MB. The writes are scattered over
the whole 10.5 GB span, so the writeback and re-dirty cycle of item 1 runs with the whole file as
its window. The two string columns took the extent route and did not pay this.

**Proposed fix.** The same shape as item 1 does not apply directly, because the target is the
whole entity space rather than a batch slice. Two routes: sort the (entity, value) pairs in
chunks and merge, writing each column sequentially, which the keyword dictionary already does for
its own spill; or map the column files with `MAP_PRIVATE` and write them out once at the end,
which costs their size in anonymous memory. The first fits the budget; the second does not at
this scale.

## 5. `layers` leaves 34 GB of freed heap resident for the rest of the build

**Measured.** 2,183 s against 2,815 s linear. Anonymous RSS went from 4.5 GB to 46 GB in
ninety seconds at the publication of the levels, swap from 0.25 to 8 GB in the same window, and
neither came back: 42.6 GB anonymous plus 10.6 GB swapped, four hours after the stage ended,
with `[heap]` a 37 GB segment of which 34 GB is resident. The log printed no "stayed on the heap"
line, so every membership was rehoused to the mapped extent and its owned bitmap dropped.

**Cause.** A level's memberships are built as owned Roaring bitmaps, about 1.6×10⁶ of them over
1.0×10¹⁰ entries, in the allocator's main arena. Freed chunks in glibc's main arena stay
resident: the allocator returns memory only from the top of the heap, and the records and key
index allocated after the bitmaps sit above them. The memory is free to the process and
unavailable to the page cache. Reasoned from the allocator's documented behaviour, not confirmed
by a heap dump; the confirming test is `malloc_trim(0)` after the rehousing and a second run.

**Proposed fix.** Call `malloc_trim(0)` after `write_membership_extents`, which the
`epoch_shard_projection` bench already does for the same reason; or route CRoaring's allocations
through an allocator that returns freed pages; or spill each level's bitmaps to the extent as they
are built rather than holding a level of them. The first is one line and should be measured first.

## 6. The keyword dictionary's scatter runs with no page cache and reads 16 TB

**Measured.** `specieskey`, 3,221,136,771 present rows, 1,398,425 keys, 56 sorted runs of about
75 MB. The runs were written in 24 min. The merge and scatter then ran from 03:42 and was 49%
through the runs at 07:35, reading 0.9 to 1.3 GB/s from disk the whole time with file RSS near
zero, 100 to 180 major faults a second, and one core at 50%. Cumulative reads over the stage: 16
TB. Projected stage time about 8 h.

**Cause.** The merge writes each row's ordinal into `keyword-ordinals.scratch`, a 12.9 GB mapped
`u32` array addressed by row, in key order, so the writes are scattered over the whole array. The
design relies on the page cache holding the array; item 5's dead heap leaves the cache 3 GB, so
each scattered write reads its page from disk and the array is read back many times over. The
alternative the design rejected, a second external sort of `(row, ordinal)`, is bounded by the
plan and does not depend on the cache.

**Proposed fix.** Take the rejected alternative: spill `(row, ordinal)` pairs in row-sorted runs
as the merge yields them and merge those into the values file. It costs 8 B a row of spill and one
more merge, and its cost does not depend on what else the build holds. Item 5's fix alone would
also restore the cache this run needed, but the scatter would still be one bad neighbour away from
the same collapse.

## 7. The tiler sort holds 12 B a row on the heap, and no model charges it

**Not reached.** `RowRec` is 12 B, and the tiler sort collects one for every row of the view into
a `Vec` before sorting: 42 GB at this corpus, anonymous, beside whatever the heap holds. The
residency model has no term for it, so the memory pre-flight admitted this build at 13.4 GB against
a 24 GB budget. On a 47 GB box this stage cannot complete with item 5's heap resident, and would
swap heavily without it.

**Proposed fix.** Add the term to the model so the pre-flight refuses with the arithmetic. Then
bound the sort: the records are in entity order and the segment wants Morton order, so a chunked
sort with a k-way merge over spilled runs, the shape the member and keyword merges already use,
holds one chunk rather than the corpus. The identity tiebreak is a pure function of the entity and
survives the split.

## 8. Measured after the fact

- **`manifests` is the artifact pass.** The stage's interval opens when `segment_write` ends and
  closes after the digests, so it reports the artifact pass, containment and the scratch sweep;
  the digests are 1.6 to 1.8% of it (0.115 s of 6.27 s at 16.3×10⁶ rows, 1.20 s of 74.2 s at
  125.8×10⁶) and already run on twelve threads. The 1,637 s linear projection in the reference
  table is the artifact pass's. Digest-on-write was modelled at about 1% of a rung-6 build and
  not built.
- **`dictionary` was a per-row string.** The access column is `RLE_DICTIONARY` with 251 values
  and a dictionary page in every row group; the reader hydrated it to one `String` a row and both
  passes did a per-row lookup, twice over the column. Read as a dictionary array with a per-batch
  resolution: at 300×10⁶ rows `dictionary` 56.8 s to 14.1 s and `geometry_read` 57.1 s to 33.0 s,
  bundle identical. Modelled at rung 6: about 580 s off `dictionary` and a comparable share of
  `geometry_read`.
- **Parquet decode is not the lever in the points passes.** `source_ids` and `geometry_read`
  already decode row groups on a pool with a bounded channel; at 300×10⁶ rows the decoders sat
  63 to 89 s a stage waiting on one consumer thread at 98%. `attribute_tail`'s decode is 16% of
  the stage at that scale and its vocabulary minting is order-bearing, so it stays single-threaded.

## 9. Open measurements

- A `--limit` build reads the whole points file: at 2×10⁶ rows of the 3.5×10⁹, `attribute_tail`
  took 210 s and read 38 GB to grow the bundle by 0.19 GB (measured, `probes/2026-09-12-bounded-assembly/`
  format check). The source ids ascend, so the scan could stop at the limit. A probe cost, not a
  rung cost, and it makes the four-prefix measurement slower than the prefixes deserve.

- Whether item 1's cost is bounded by the slice size or keeps rising with the dirty set. Batches
  seven to ten will say.
- Every stage after the batch loop against the reference table above; none has a measurement
  above 125.8×10⁶ items.

## What the second build showed (2026-09-13)

Two whole-corpus builds over the same 3,495,729,729 placed rows and the same ten-batch plan: run 1
on main `a4152e79` (this memo's own build), run 2 on main `d7d26c16` after the bounded-assembly
design (`2026-09-12-bounded-assembly-design.md`) and its six branches merged. Full figures are
[`../../ingest-campaign.md`](../../ingest-campaign.md) §4d. This section answers each item above
against the design's fix, and closes with two findings the second build itself produced.

**Item 1 (the assignment walk's writeback) — the hoist landed and the diagnosis needs a
correction.** The label-agreement check moved into ordinal order, which removes the scattered
entity-map writes: a late batch that wrote 200–380 MB/s and faulted 50,000–90,000 pages a second
under run 1 wrote 25 MB/s and faulted a few hundred pages a second under run 2. That took about
12% off the loop total, not the loop's whole climb. What remains is Finding B, below.

**Item 4 (`attribute_tail`'s scattered value-column writes) — not fixed in this merge set.**
`attribute_tail` fell from 1,646 s to 1,574 s, a 4% change consistent with noise rather than with
the `(entity, value)` partition the design proposes (§4.3): none of the six branches that landed
is that partition. The scattered write-and-refault cycle this item describes is still in the
build.

**Item 5 (`layers` leaves freed heap resident) — did not recur.** `layers`' anonymous peak under
run 2 was **9.1 GB**, against the 46 GB anonymous plus 8 GB of swap this memo measured. The
batched publication (design §4.5, at most a few million membership entries a batch) and
`malloc_trim(0)` at stage boundaries between them removed both the peak and the retained heap.

**Item 6 (the keyword dictionary's 16 TB scattered read) — gone.** `filter_postings` fell from
3,677 s to 1,606 s, with reads of about 25 GB against the 16 TB this memo measured. The
`(row, ordinal)` partition (design §4.2) replaced the scattered write into
`keyword-ordinals.scratch`, so the merge's output no longer depends on how much page cache the
rest of the build leaves it.

**Item 7 (the tiler sort's unmodelled 42 GB) — ran within budget.** `tiler_sort` completed at
**328 s**, up 86 s from run 1 with no code change on that stage (unattributed; assumed page
cache) but nowhere near the 47 GB box this memo said could not complete it. The row partition
(design §4.1) bounds the sort by bucket rather than holding one record a row on the heap.

**Item 8 (measured after the fact) — the artifact pass now has its own stage record, as
proposed.** `manifests`' interval no longer reports the artifact pass: the two are timed
separately in run 2 (artifact pass 1,901 s, `manifests`' own digests 69 s), which is the split
§4.6 of the design asked for. The per-batch dictionary read (56.8 s → 14.1 s at 300×10⁶ rows) is
not part of this merge set — `dictionary` and `geometry_read` moved in the slower direction
between the two runs (162 s → 189 s, 398 s → 476 s), unexplained and not attributed to that fix,
which was never landed here.

### Finding A — `filter_postings` still exceeds the budget, and the design's rule was not applied to it

**Measured.** Anonymous RSS reached **31.6 GB, 7.6 GB over the 24 GB budget, for about twenty
minutes** in `filter_postings` under run 2 — the one point at which the post-design build did not
fit.

**Cause.** `ExtentColumn::open` (`crates/tessera-build/src/extents.rs`) deserialises every
extent's has-row bitmap onto the heap and builds a per-extent live set by subtracting later
extents' rows (`andnot_inplace`). Each of the two string columns (`scientificname` and
`specieskey`) spilled 988 extents; the has-row files are run-encoded on disk (2.2 GB for
`scientificname`), but the subtraction yields array containers at 2 B an entity — about 7 GB of
live sets a column, plus about 4.5 GB of has-row for the two columns, over a 6 GB base. Run 1's
fold to 8 extents put 437 M entities in each extent, so every container was a fixed 8 KiB bitset:
about 14 GB for both columns, half of run 2's figure. The fold under run 1 halved the term by
changing the container's encoding, not by bounding it, and the string-column extent fold being
bounded by the budget instead of a fan-in of 128 (one of the six merged branches) meant no fold
ran at rung 6 at all, so the unbounded shape is what run 2 measured in full. `merge_fan_in`'s
comment assumes a join chunk covers a contiguous entity run; a chunk is in the attribute source's
order, scattered over entity space, so that assumption is false and the residency model has no
term for the live-set structure this produces.

**Proposed fix.** Open extents cursor-only with the has-row file mapped, so the sequential cursor
takes each row's entity from the block; replace the per-extent live sets with one duplicate map a
column — `seen` and `repeat` bitmaps plus the last extent index per repeated entity, built in one
sequential pass over the has-row files, about 90 s here — and add the one-bitmap term to the
residency model. Modelled cost after the fix: about 1 GB for both columns, no disk, and about no
change to the pass's wall time. Not built.

### Finding B — the assignment climb is not writeback, and needs a profile

**Measured.** After item 1's hoist, an assignment batch still climbs from 65 s to 165 s with the
batch index, on one core at 100% with no disk traffic. The climb is superlinear in batch size: the
half-size tenth batch costs 0.25 µs an item against 0.45 µs an item for a full batch.

**Correction to this memo's item 1.** The scattered writeback this memo diagnosed as the
assignment walk's main cost is gone, and about 12% of the loop's wall time went with it — the
remaining 88% was never writeback. What binds now is CPU work that grows with the batch index and
worse than linearly with batch size, and this memo has no measurement of what that work is.

**Next step.** A `perf` profile of a late batch on the prefix ladder, not another guess. Chunking
the walk across cores by position (this memo's item 1, second step) stays open behind that
profile: it addresses writeback, and writeback is no longer most of the cost.

## Third build (2026-09-14)

A third whole-corpus build, main `488e43e5`, after the merge of Finding A's fix (extents opened
cursor-only with the has-row file mapped, one duplicate map a column) alongside three serve-path
changes: candidacy off the cached histogram with block-wise mask scans, a windowed session
projection with a whole-grant short-circuit, and a per-segment cut index (`cuts.u32`, bundle
format 11) with selection evaluated per cell on dense tiles. Full figures are
[`../../ingest-campaign.md`](../../ingest-campaign.md) §4d.

| stage | second build | third build |
|---|---|---|
| `source_ids` | 93 s | 81 s |
| `dictionary` | 189 s | 156 s |
| `geometry_read` | 476 s | 354 s |
| batch loop | 1,463 s | 1,255 s |
| `postings_write` | 63 s | 59 s |
| `attribute_tail` | 1,574 s | 1,331 s |
| `layers` | 2,258 s | 1,881 s |
| `filter_postings` | 1,606 s | **1,076 s** |
| `record_blob` | 1,350 s | **806 s** |
| `tiler_sort` | 328 s | 208 s |
| `segment_write` | 629 s | 577 s |
| `artifact_pass` | 1,901 s | 1,815 s |
| `manifests` | 69 s | 69 s |
| **wall** | 3 h 30 m 55 s | **2 h 52 m 33 s** |

Finding A's fix holds at scale: `filter_postings`'s own stage peak fell to **8.9 GB anonymous**,
against 31.5 GB under the second build, inside the 24 GB budget. Part of every stage's gain is the
idle box (the second build shared it with other work); the `filter_postings` and `record_blob`
gains are the fix's, `record_blob` now having the page cache the live sets had held.

### Finding C — hot zoom 0 falls 4 to 6× on a scattered mask, and stays disk-bound on a dense one

**Measured.** Under the battery (`serve_battery.py --view geo --zooms 0,6,12 --deciles 9
--candidates 40 --samples 10 --cold-samples 3 --text-samples 0`), hot zoom 0 for the sparse
principals fell 4 to 6×: 268 → 57 ms at 1%, 1,269 → 231 ms at 5%, 2,653 → 453 ms at 10%, about
1,060 ms at 25%. It stays linear in visible rows, at about 1.2 ns a row. For the dense principals
(50%, 100%) hot zoom 0 is 10 to 17 s, barely better than the roughly 20 s the second build's
single request measured.

**Cause.** A scattered mask takes the sparse `Values` decode tier, which keeps the per-row
identity scan: the per-cell route applies to dense tiers only. For the dense principals, the
sampler shows the server reading 2 to 3.3 GB/s from disk during the request, with 8.7 GB of file
pages resident beside 15.7 GB anonymous under the 24 GiB cap. The per-cell route probes the
identity column inside every occupied cell, and a leaf cell's 83 ids span 664 B, so every 4 KiB
page of the 28 GB identity column holds several cells and every page is touched. Under the cap the
column cannot stay resident, so the route is disk-bound whether it probes per cell or per row.
Modelled resident cost is 0.2 s for the per-cell route against 2.8 s for the per-row route; the
cap turns both into a 28 GB read.

**Directions, open at the time of writing.** A narrower identity column; a resident per-cell
summary that answers the cut without touching the column; a larger cap on a larger box; a change
to what §7.2 requires at whole-map zoom.

### Finding D — the server's resident memory grows over a battery run, and the growth is not yet explained

**Measured.** The server's anonymous memory rose from 6.7 GB at open to 15.7 GB after the six
principals' battery sessions: 10.1 GB anonymous mappings and 5.5 GB heap. Not yet attributed.

### ⊘ Two things noted, not investigated

The battery harness authorises once and never re-authorises; the deployment's token lifetime was
raised to 43,200 s for this run, as it was for the second build's second attempt. Two other cargo
builds ran on the box during the battery.

W1 of the layers investigation (item 6, above: reading the member key column as its own parquet
dictionary) measured a 31% row-loop gain in the investigation and a 4% loss in the
implementation, on the same prefix (both measured, `gbif-64p`, 2026-09-14). Held as a patch for an
A/B at rung 6, not built here.

## Fourth serve (2026-09-14)

**Finding D, resolved.** The 9 GB rise was glibc's free pool: 14.15 GiB of freed memory held in
373 arenas plus a 5.28 GiB main arena, because nothing on the serve path called `malloc_trim`; the
server's own cache accounting held 127 MB. A merge adds `malloc_trim(0)` on a cadence, an
`M_ARENA_MAX` cap sized from the thread count, and the figures on `/control/status` that this
diagnosis needed. Served a fourth time on the same bundle: peak anonymous memory fell to 10.7 GB,
against 16.0 GB under the third serve.

The dense-principal result changes with it. Under the third serve both the 50% and 100% principals
were disk-bound on the 28 GB identity column and hot zoom 0 was 10 to 17 s at both. Under the
fourth serve the 50% principal, whose grant is contiguous in Morton space and so decodes as runs
onto the per-cell route, falls to 2.2 s: its half of the identity column, 14 GB, now fits in the
page cache the freed memory no longer holds. The 100% principal touches the whole 28 GB column and
stays disk-bound, 16.9 s → 14.6 s. Full figures are
[`../../ingest-campaign.md`](../../ingest-campaign.md) §4d.

This is the mechanism, not a fix for it: the identity column's residency under the cap is still the
constraint the 100% principal hits. A proposal for removing that dependence, and the experiment to
run before any design is written, is
[`2026-09-14-whole-map-selection-under-a-cap.md`](2026-09-14-whole-map-selection-under-a-cap.md).
