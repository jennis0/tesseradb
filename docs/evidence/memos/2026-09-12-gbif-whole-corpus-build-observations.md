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

## 8. Open measurements

- A `--limit` build reads the whole points file: at 2×10⁶ rows of the 3.5×10⁹, `attribute_tail`
  took 210 s and read 38 GB to grow the bundle by 0.19 GB (measured, `probes/2026-09-12-bounded-assembly/`
  format check). The source ids ascend, so the scan could stop at the limit. A probe cost, not a
  rung cost, and it makes the four-prefix measurement slower than the prefixes deserve.

- Whether item 1's cost is bounded by the slice size or keeps rising with the dirty set. Batches
  seven to ten will say.
- Every stage after the batch loop against the reference table above; none has a measurement
  above 125.8×10⁶ items.
