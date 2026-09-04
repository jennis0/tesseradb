# Row projection per epoch shard: is it linear in rows, and what do N leaves per token cost?

**Status:** measurement, 2026-09-04. Synthetic, in memory, no bundle. Host: WSL2 on a 12-core
Ryzen 9 5900X (512 KB L2 per core, 32 MB L3 visible), 47 GB, kernel 6.18; every run
single-threaded under `nice -n 10` while another agent's rung 5 ingest (one core, ~10 GB) ran
beside it. Harness: `crates/tessera-bench/src/bin/epoch_shard_projection.rs`; sequence: `run.sh`;
tables: `collate.py`; record: `result.json`.

LEAD

## The question

Splitting a corpus into N epoch shards inside one process gives each session N row projections
(one per shard) instead of one. That is a saving only if `Permutation::project` is linear in the
permutation's size, so that a shard at 2³⁰ rows costs about 1/N of the corpus figure; and it is
affordable only if N small projections per token — held for as long as the session lives — cost
little more than one large one in time and in memory.

The corpus figure is **1 277 ms** for `Permutation::project` at 10⁹ rows: measured
single-threaded, median of 3, over a 25 % grant (2.5×10⁸ entities chosen uniformly at random) and a
scattered permutation (a cycle-walked Feistel bijection, uncorrelated with entity order), on this
host with nothing else running (`../2026-08-14-project-decomposition/1e9-scattered.txt`, line
`Permutation::project  median 1277.3 ms`). The same run gives 130.6 ms at 10⁸ under the same
conditions. Both are wall clock. The engine cites the 10⁹ figure at `RowProjection::new`
(`crates/tessera-engine/src/compose.rs`) and in the session's projection-cache commentary
(`session.rs`), and it is the cost the refresh ladder, the merge patch and the fold's stampede
rules are all priced against.

## Method

Every configuration is a fresh permutation file written through `PermutationWriter` (sequentially:
slot `e` gets the shuffle's image of `e`), opened with `Permutation::load` and checked with
`validate_rows`, then projected through the public `Permutation::project` — the production path,
untouched. Masks are entity-space bitmaps: **scattered** puts each entity in independently with
probability `coverage`; **contiguous** is one run of `coverage × rows` entities at a random
position, run-optimised. One untimed warm-up, then five timed calls; the median is the figure.
Around every timed call the harness also reads the process's CPU time, page faults and context
switches, and four system-wide `/proc/vmstat` counters, because the box was shared and a wall-clock
sample can be the scheduler's rather than the algorithm's.

- **(a) Linearity.** Scattered permutations at 10⁶, 10⁷, 10⁸ and 4×10⁸ rows (the 4×10⁸ file is
  1.6 GB); masks at 25 %, 10 % and 1 %. The 25 % arm exists so the fit can be compared against the
  recorded figure under its own conditions; 10 % and 1 % are the design's. Each size ran in its own
  process, twice, not back to back. At 10⁸ the contiguous mask was run beside the scattered one.
- **(b) One mask, one permutation against eight.** 10⁸ rows as one permutation and as eight of
  1.25×10⁷ with entity ranges split contiguously; the same mask projected both ways. The public
  `Permutation` has no entity offset, so a shard's permutation covers local ids and the global mask
  is restricted to the shard's range and shifted down (`and` with a range, `add_offset`) before
  projecting; a shard-aware permutation would seek into the mask as `SegmentExtent::project` does
  and pay none of that, so the split is timed apart and reported beside the projection.
- **(b) Per token.** 10⁴ tokens' leaf projections over a 10⁶-row universe at 1 %, each token a
  distinct mask, all held at once: 10⁴ bitmaps in the one-permutation shape against 8×10⁴ in the
  eight-shard shape. One shape per process. Resident set is `VmRSS` from `/proc/self/status` read
  after a discarded warm-up token and `malloc_trim`, and again after the loop; beside it the
  allocator's own in-use bytes (`mallinfo2`: arena in-use plus mmapped chunks) and, for the
  bitmaps themselves, croaring's portable serialised size and its `statistics()` container bytes,
  plus `size_of::<Bitmap>()` per bitmap. Neither of the last two counts croaring's per-container
  index arrays, which is what the resident-set delta adds.

TABLES

## Caveats

- **The box was shared, and it shows in the wall clock.** Another agent's rung 5 ingest ran
  throughout (one core at nice 10, ~10 GB resident, streaming writes into the page cache), and its
  builds ran intermittently at nice 0 on every core. A step that overlapped a build measured the
  scheduler: at 4×10⁸ over 25 % one process saw wall clock 1 455 ms against 791 ms of CPU time with
  no major faults, which is preemption of a nice-10 process, not `project`. `run.sh` therefore
  waits for no compiler and a one-minute load under 5 before each step, every size ran twice, and
  each sample carries its own CPU time and switch counts so a contaminated one is recognisable.
  The tables use the less-interfered process; the other is shown beside it.
- **Memory bandwidth and the shared L3 were never idle**, so even CPU-time figures at the sizes
  whose working set leaves the cache are an upper bound on the idle-box cost. The recorded 10⁹
  figure was taken on an idle box.
- **The fixture is synthetic.** A scattered bijection and a uniformly random mask are the shape
  `permutation.rs` designs against and the shape the recorded figure used; a real fragment is
  signature-sorted and partly contiguous, which the contiguous arm bounds from the other side.
- **The sharded arm's split is an artefact of the emulation.** Its cost is reported so it can be
  subtracted, not because a shard-aware permutation would pay it.
- **Per-call fixed cost.** `Permutation::project` allocates and zeroes a 512 KB stamp array and
  one bucket vector per 2²² rows on every call, whatever the mask's size. That is the term the
  per-token comparison exposes at N = 8, and it is a property of the implementation rather than of
  sharding.

## Reproducing

```bash
CARGO_TARGET_DIR=/home/joe/code/tessera/target cargo build --release -p tessera-bench \
    --bin epoch_shard_projection
bash probes/2026-09-04-epoch-shard-projection/run.sh all      # ~15 min on a quiet box
python3 probes/2026-09-04-epoch-shard-projection/collate.py   # result.json and the tables
```

The 4×10⁸ step needs 1.6 GB of scratch disk and peaks under 3 GB resident; the whole sequence
stays under 4 GB.
