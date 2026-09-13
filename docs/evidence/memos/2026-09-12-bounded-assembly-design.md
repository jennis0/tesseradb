# A build whose memory does not scale with the corpus

**Date:** 2026-09-12
**Status:** Built and merged on main at `d7d26c16`, measured at rung 6 on 2026-09-13 —
[`../../ingest-campaign.md`](../../ingest-campaign.md) §4d. It answers the findings of
[`2026-09-12-gbif-whole-corpus-build-observations.md`](2026-09-12-gbif-whole-corpus-build-observations.md)
with one rule and one primitive, and names the work packages that apply them. §9 lists the
decisions it asked for.

## 1. What the whole-corpus run showed

Rung 6 is 3,495,729,729 rows. On a 47 GB box the build ran seven hours and could not have
finished. The stages before `layers` were linear in the row count. From `layers` on:

- `layers` built each level's memberships as owned Roaring bitmaps, about 12 B an entry for a
  level whose members are scattered across entity space, and `prepare_publish` took its own
  copy before the first set was dropped. The peak was 46 GB anonymous plus 8 GB of swap for a
  level of 3.4×10⁹ entries, against a model term of 4 B an entry. After the level was written
  and its bitmaps freed, glibc kept 34 GB of that heap resident for the rest of the run, and
  the page cache was left with 3 GB.
- The keyword dictionary scattered ordinals into a 12.9 GB mapped array in key order. With no
  cache, each scattered write read its page from disk: 16 TB read in four hours for 49% of the
  stage.
- The tiler sort collects one 12 B record per row into a heap vector, 42 GB, beside the
  row→entity and residual vectors at 14 GB each, and then builds the `tessera_id` column as a
  28 GB vector. A 70 GB peak. The residency model has no term for any of them, so the memory
  pre-flight admitted the build at 13 GB.

Three smaller findings share a mechanism with the second: the assignment walk, the attribute
tail and the entity-order geometry each write a mapped file at a scattered index, and every page
of those files is written back and re-dirtied many times. Measured: 123 GB written to grow the
bundle by 34 GB in `attribute_tail`, and 15× on the entity map in the batch loop.

The disk-use campaign of 2026-09-10 removed the heap arrays it found by making them mapped
files. That stopped one step short: a mapped file written at a scattered index is bounded in
memory only while the page cache holds it, and the cache is whatever the rest of the build
leaves.

## 2. The rule

**No structure whose size is the row count is anonymous memory, and no mapped file is written
at a scattered index.** Three consequences:

1. A row-sized array is a mapped scratch file written front to back, or it is not materialised.
2. A pass that produces values in one order and needs them in another goes through a
   **partition**: the values are appended to key-range buckets on disk, and each bucket is then
   read whole into a window, put in order there, and written out sequentially. Memory is one
   bucket and one window; disk is one copy of the values, released bucket by bucket.
3. Freed heap is returned to the system at every stage boundary.

One exception is named, because it is a rate over the row count and the rule says there is
none: writing `columns.arrow` in place runs Arrow's writer over the real row count, and the
writer allocates an all-ones validity bitmap of `n / 8` bytes for every column, all alive at
once, 2.2 GB at rung 6. The model charges it and the model's test subtracts it. It is the price
of one encoder rather than two. A second exception is the artifact pass's bucket on a list-form
level: with 128 buckets it holds one record per member entry in its row range, which no type
bounds, so a level of many entries a row has a bucket the model charges at the largest layer's
entries over 128 (12 GB at rung 6 for a MeSH-shaped level; GBIF's taxonomy is label-form and
pays 268 MB). Bounding it means a bucket count derived from the entries, which §3 does not do.
Open.

Two models read the result and they are not the same model. The **batch plan** (`loop_fixed`,
`per_batch` in `plan_build`) sets the signature stride, the stride partitions entity-id space,
and a different stride is a different entity-id assignment, which I9 forbids. Its arithmetic
is frozen: this design adds nothing to it and removes nothing from it. The **entity-order
model** (`residency::routes_for`, the `tail` the pre-flight refuses on) is an inventory of what
the stages hold, and this design makes that inventory complete: every anonymous term that
remains is in it, and a test asserts the total does not grow with the fixture's row count.

The build already partitions in two places: the signature batch loop buckets its pairs by
ordinal range and loads one at a time, and the member and keyword passes spill sorted runs and
merge them. What follows applies the same shape to the places that do not.

## 3. The primitive

`spill::Partition`, generalising the batch loop's `BucketSink` and `BucketStore`:

- `Partition::create(dir, name, boundaries, record_width)`. Records are fixed-width byte
  strings whose first four bytes are a `u32` key. `boundaries` is the ascending list of the
  first key of each bucket; a record is routed by binary search over it. One buffered writer per
  bucket, 1 MiB each, appending sequentially.
- `push(record)`, `finish() → PartitionStore` with the count and anchor the spill receipts
  carry, `load(k) → Vec<u8>` read once and verified against the receipt, `delete(k)`.
- `boundaries_uniform(key_bound, buckets)` for a key that is dense and uniform, such as an
  entity or row index, and `boundaries_from_histogram(counts, target)` for one that is not.

**Bucket count is 128, always.** Entity space is `u32`, so a bucket of a uniform key holds at
most 2³² / 128 = 33.6×10⁶ records, 537 MB at 16 B, and its window at most 33.6×10⁶ × the
value width. The partition's memory is then bounded by the type and not by the corpus or the
budget: one bucket, one window, 128 MiB of writer buffers. The pre-flight charges it as a
constant and prints it. A budget too small for that constant, 2 GiB is the floor, refuses with
the arithmetic. This is the reason to fix the count rather than derive it: a derived stride is
one more term the model can get wrong, and the u32 ceiling makes the fixed one cheap.

**Uneven keys.** A partition by Morton code sets its boundaries from a histogram: one pass
counts `morton >> 8`, and the boundaries are the smallest prefixes whose count fits the target
of `n / 128`. A bin is the sum of up to 256 cells, so a hot locality, and GBIF has cells of
millions of identical coordinates, can put a single bin over the target. Such a bin is refined
by a second count over its full 32-bit codes, and a single code over the target is split by
`priority` range, which is sound because the row order is `(morton, tessera_id)` and `priority`
is the identity's prefix. A single `(morton, priority)` pair over the target is a refusal with
the arithmetic; it needs 33.6×10⁶ points at one cell sharing sixteen random bits, which no
corpus has. The plan prints the largest bucket.

## 4. Where it applies

### 4.1 The tiler sort and the segment write

The assembly of a view becomes one pass over Morton-partitioned rows fed straight from the
ordinal-order files. The entity-order geometry files `x-of-entity.u32` and `y-of-entity.u32`,
which nothing outside this stage reads, are never written, and the walk that scattered them
goes.

1. **Histogram.** One sequential walk over `x-of-ordinal`, `y-of-ordinal` and the view's
   ordinal presence counts `morton >> 8`, refined as §3 says, and fixes the Morton boundaries.
2. **Row partition.** The same walk, run again, pushes a 12 B record per present ordinal:
   `morton`, `residual`, `entity` from `entity-of-ordinal`. The residual is carried so that no
   later pass reads geometry at a scattered index. `priority` is not carried: it is
   `forward(entity)`'s prefix and is computed once per record when a bucket is loaded, which is
   eight `splitmix64` rounds a row on a parallel iterator. The view's ordinal geometry is
   unlinked when this walk ends. The walk is single-threaded where today's row build is
   parallel; at rung 6 it writes 42 GB at the writer's speed, a figure to take rather than to
   design around.
3. **Per bucket, in Morton order.** Load, compute each record's priority, sort by
   `(morton, tessera_id)` through the comparator the record already has, `priority` as the
   prefix and `forward(entity)` on a prefix tie, then emit each row in order: `morton.u32` and
   `row-entity.u32` are appended, they are raw little-endian and the scratch is the bundle
   file; `forward(entity)` and the residual are appended into `columns.arrow`'s body at their
   precomputed offsets (step 6); the occupancy counter is fed the codes as a stream, as today;
   and `(entity, row)` is pushed to a second partition by entity range. Each row bucket is
   deleted once loaded, so this partition shrinks by 12 B a row while the next grows by 8.
4. **The permutation.** Each `(entity, row)` bucket is scattered into a window and written
   into the permutation file as one sequential run. `PermutationWriter::set` accepts any order
   and the page plan is derived from the view's presence bitset, which exists, rather than from
   a second walk of the rows. The scattered and sequential constructors are already pinned
   byte-identical by a test.
5. **The render tail.** A render column is gathered by entity and needed by row, a scatter in
   each direction. Each `(entity, row)` bucket from step 4 reads the column sequentially and
   pushes `(row, value)` to a partition by row range; each row bucket writes its window into
   the column's buffer in `columns.arrow` at its offset. Two partitions of 8 B a row for a
   column of any width, the second growing as the first is consumed. The design does not
   gather directly when the column would fit the cache; that branch is the cache-dependence
   being removed.
6. **`columns.arrow`.** The reader requires exactly one record batch with contiguous
   8-byte-aligned buffers, and the serving design depends on that. The file is written in
   place rather than through Arrow's writer: its layout is a function of the schema and the
   row count alone, since every length in the IPC metadata is a fixed-width integer, so the
   schema message, the record-batch metadata, the body offset of every buffer and the footer
   are computable before the first row is emitted. The file is reserved at its final size, the
   buffers are filled at their offsets by steps 3 and 5, and the framing is written around
   them with the flatbuffer builders the `arrow-ipc` crate exposes. Byte identity with the
   writer it replaces is asserted on every fixture in §5, and the validity bitmaps the reader
   tolerates and nothing reads are written as they are today.

**Memory.** One bucket and one window per partition in flight, two at the peak (steps 4 and
5), the occupancy state. Nothing sized by `n`.

**Disk, at rung 6.** The row partition stands whole at 42 GB when step 2 ends, with the
28 GB of ordinal geometry just released, and is consumed while the `(entity, row)` partition
grows. That partition is read once, its buckets deleted as they are loaded, and every render
lane is filled from the same pass, so the pairs shrink at 8 B a row while the lanes grow at
`Σ(4 + wᵢ)` B a row; for `kingdom` alone the lanes end at 17 GB with the pairs gone. The
stage's transient is the row partition's 42 GB. Against today: the 28 GB of entity-order
geometry is not written and the 42 GB of column scratch a vector-backed writer would need does
not exist. The stage's transient rises by 14 GB at rung 6; §7 has the phase arithmetic.

### 4.2 The keyword dictionary's ordinals

The merge yields `(row, ordinal)` in key order and pushes it to a partition by row range. Each
bucket is scattered into a `u32` window and pushed to the values writer in `VALUE_CHUNK` slices.
`keyword-ordinals.scratch` goes. The row-bound check on each write becomes a bucket-range
check, and both count guards stay. Each row's ordinal is written exactly once, so the order of
writes cannot change the file. This is the alternative the pass's documentation rejected, and §1
is the reason to take it.

### 4.3 The attribute tail's value columns

The join resolves each chunk to `(entity, staged position)`, sorts it stably by entity, and
scatters every lane into its mapped entity-order column. The fixed-width value lanes instead
push `(entity, value)` to a partition by entity range, one per column at the column's width,
and after the join each bucket is scattered into a window and written into the column file as
one sequential run, with the presence bits set in a window bitset written the same way. A
bucket is replayed in append order, which is sweep order within a chunk and chunk order across
chunks, and that is the order the stable sort gives today, so which of two writes to one entity
wins is unchanged.

The per-chunk sort stays. The extent lanes push one extent per resolved row in the sorted
order and deduplicate against the preceding entity, so their bytes depend on it. The arena
route, a string column whose characters fit the disk as an arena, is taken whenever the disk
allows, so its offset lane goes through a `(entity, offset)` partition at 12 B a row the same
way; the arena itself is appended in arrival order. Charging the arena route those 12 B a row
moves the route choice for a string column near the free-space ceiling to the extents, which is
a different bundle for that column and the right one. An absent row pushes nothing, as today.

### 4.4 The entity map in the assignment walk

A batch's ordinal range is contiguous, so this is a window without a partition: the walk fills
a heap `Vec<u32>` of the batch length at the record's local index and writes it into
`entity-of-ordinal.u32` at the batch's offset in one call. The batch plan's retained 4 B an
item, charged for a tally that is now a file, pays for it: the vector is `batch_items × 4 ≤ n
× 4`. The plan's arithmetic does not change. Chunking the walk across cores by position is a
separate change after this one is measured.

### 4.5 The layers stage

**The publication is batched.** A level is published in batches of artifacts in the level's
order, each batch as many artifacts as fit a share of the budget at 24 B a member entry, two
copies at the measured 12 B, and never fewer than one. Sized by entries rather than by
artifact count because the family level is 11,502 artifacts over 3.4×10⁹ entries and the
species level 1.4×10⁶ over the same. A larger batch buys nothing past a few million entries:
the work per artifact is the same in any batch, and the batch is already built in parallel
across the cores. Each batch's bitmaps are built from
the mapped member table, `prepare_publish` and `store.apply` run on the batch, the batch's
records are encoded straight into the level's membership pack, whose writer streams, and the
records' owned bitmaps are replaced by a placeholder until the finished pack is mapped and
rehoused as today. Ordinals are dense in the level's order and artifact ids allocate in that
order, so the assignment is the batching-independent function it is now; the identity test in
§5 asserts it. The entity-order model's term for a level, 4 B an entry, becomes one batch's
entries at the measured 12 B, which is the bound the stage then has.

**The heap is returned.** `malloc_trim(0)` after the rehousing and at every stage boundary in
`build_observed`. The 37 GB `[heap]` segment says the bitmaps were arena chunks, which is what
Roaring containers are below the mmap threshold, and glibc releases free pages in every arena
on trim. Fragmentation against the records allocated above the bitmaps is the residual risk,
which the batching bounds and the measurement in §7 checks.

### 4.6 The artifact pass, and the fold with it

`project_row_column` builds a level's row column by walking each artifact's row bitmap and
writing the ordinal at every row into a row-sized array, 14 GB a level at rung 6, then packs
it. The engine composes the same column at every fold through the same function. Owner ruling,
2026-09-12: **one implementation, disk-backed on both sides.** The partition primitive moves to
the store crate; every membership entry is pushed as `(row, ordinal)` to buckets by row range,
and each bucket is sorted by `(row, ordinal)` and replayed into the packed writer, which writes
the column front to back into the file it becomes. The sort is what makes the list form's byte
order a property of the replay: a row's ordinals ascend because both walks hand them ascending,
and now because they are sorted. The build's buckets live under `.build-tmp/`; the fold's under
the deployment's cache directory, and an open sweeps what a crashed fold left. The fold's cost
becomes 8 B an entry written and read once — twice for the list form, whose offsets need a
counting pass over the buckets before any value can be placed — in place of a 4 B a row heap
array and two passes,
and its memory is one bucket. The other two routes considered: one byte writer fed by two
traversals, which leaves the fold's memory as it is; and the partition with heap buckets on the
engine, which costs 8 B an entry there. Neither taken.

The artifact pass also gets its own stage record, since `manifests` today reports its interval.

### 4.7 The rest

`appearances` becomes a mapped scratch file beside the tally, and the batch plan's retained 4 B
term is documented as standing for the §4.4 window. The view's presence bitset and the render
presence bitmaps, `n / 8` each, stay anonymous and are charged. `tessera --version` prints the
commit it was built from and the build's first log line records it.

## 5. Identity

None of this changes a byte of the bundle. The entity assignment is the same walk in the same
order under a plan whose arithmetic is unchanged; a column's values are the same values at the
same entities under the same last-write rule; the keyword ordinals are the same dictionary
positions; the tiler order is a total order the partition only splits into ranges of; the
artifact ordinals and ids allocate in the same order. The assertion is byte identity against
main on `gbif-64p`, `treeoflife-1m` and `medcpt-1m`, with `diff -rq` excluding the manifest's
timestamp, and again under a memory budget small enough that every partition, batch and
publication batch is exercised on each. `gbif-64p` builds in one signature batch, so the small
budget is the run that checks the plan is untouched.

## 6. The residency model

Terms removed from the entity-order model: the tiler's records, the row→entity, residual and
`tessera_id` vectors, the keyword scratch, `appearances`, the artifact pass's lanes. Terms
added: one bucket, one window and the writer buffers per partition in flight, a constant per
the u32 ceiling; the publication batch at 12 B an entry. Disk terms added: each partition's
buckets and the segment scratch of §4.1, with their phases. The memory pre-flight's total is
then the anonymous memory the build holds, and a test builds a fixture at two row counts and
asserts the anonymous terms are equal.

## 7. Disk at rung 6

The forecast before this design was 436 GB against 415 GB available, in the assembly phase.
Three of its terms are ceilings that measure near zero on this corpus: the record blob's
blocks at half the characters where 0.26 is measured; `dict.bin` at 30 GB, 2.5 B a key over
one key a row, where 1.9 MB was written for 1.4×10⁶ keys; and `pairs.parquet` at 14 GB where
the relation packs to kilobytes. Modelled from the measured figures, the assembly peak is about
340 GB.

What this design moves, by phase:

| phase | today | this design |
|---|---|---|
| join | the value columns scattered in place | + `(entity, value)` partitions, 5 B and 6 B a row for `kingdom` and `year`, 38 GB, consumed into the same column files; the phase's peak, 281 GB forecast, stays below assembly's |
| index | `keyword-ordinals.scratch`, 13 GB | + `(row, ordinal)` partition, 26 GB; 335 GB forecast, below assembly's |
| assembly | entity-order geometry 28 GB, and 70 GB of heap | row partition 42 GB while the ordinal geometry is released; the pairs and the render lanes after it never exceed it: **+14 GB** |

`--no-oracle-pairs` is the campaign's setting, the file serving only the test oracle, and the
model already drops its term under the flag (−14 GB). The `dict.bin` term stays a ceiling of one
key a row: no distinct-key estimate exists before the dictionary is built, one taken from a
sample errs low, and an operator hint is complexity nobody should carry. The forecast is a
warning and the build goes on. The forecast the plan prints for rung 6 is then about 436 GB
against 438 GB free, and the modelled peak about 354 GB. No archive and no second volume.

**The entity-id assignment stays.** Assigning entities in Morton order would make row order
and entity order agree and remove the permutation, but entity order is signature-major so that
a term's postings are runs and a masked count is bitmap arithmetic; that is architecture, not
this design's to spend.

## 8. Verification

The 125.8×10⁶-row prefix of `data/ladder/gbif` under the 10 s sampler, before and after:
anonymous RSS against the budget, process write bytes against bundle growth, major faults,
cumulative reads, and each stage's wall time. Acceptance: write bytes within 1.5× of growth in
every stage, no major faults, anonymous RSS under the budget throughout, and the bundle
byte-identical. Then the whole corpus with the disk decision taken.

One worker, one worktree, the packages in this order, with the §5 identity tests run after
each and one full gate at the end:

| | scope | why this order |
|---|---|---|
| A | §4.5, §4.7 | without the batched publication the pre-flight refuses rung 6 outright once the term is honest, and without the trim nothing after `layers` has a cache |
| B | §3, §4.1, §4.6 | the stage that cannot complete at rung 6 under any budget |
| C | §4.2, §4.3 | the eight-hour stage and the 3.6× write amplification |
| D | §4.4 | the smallest gain; last |

## 9. Decisions

- **Disk.** Taken by the design: `columns.arrow` written in place, `--no-oracle-pairs`, no
  archive and no second volume. The `dict.bin` ceiling stands (owner ruling, 2026-09-12: no
  operator hint).
- **The render gather.** Always two partitions (§4.1 step 5), or direct when the column fits
  a share of the budget. The design says always; at rung 6 it costs nothing at the peak.
- **The heap.** `malloc_trim` at stage boundaries, measured, before any allocator change.
- **The publication batch.** Sized by entries to a share of the budget, printed with the plan.
