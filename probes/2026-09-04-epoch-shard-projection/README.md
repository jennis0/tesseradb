# Row projection per epoch shard: linear in rows, and eight leaves per token

**Status:** measurement, 2026-09-04, at commit a75e6c3b (main 4b6d8187 merged into the probe
branch; the harness is in this probe's own commit). Synthetic, in memory, no bundle. Host: WSL2
on a 12-core Ryzen 9 5900X (512 KB L2 per core, one 32 MB L3 visible to the guest), 47 GB,
kernel 6.18. Every run is single-threaded under `nice -n 10`. Another agent's rung 5 ingest ran
beside every step (one core, about 10 GB resident). Harness:
`crates/tessera-bench/src/bin/epoch_shard_projection.rs`; sequence: `run.sh`; raw output of each
step: `runs/`; tables: `collate.py`; every sample, collated: `result.json`. Every figure is
measured unless marked modelled or derived.

**(a) `Permutation::project` is linear in rows from 10⁸ upward, and a line fitted over 10⁷ to
4×10⁸ predicts the recorded 10⁹ figure within 1 %.** At a 25 % grant the fit gives 1,291 ms at
10⁹ against the recorded 1,277 ms, and the 10⁸ point here is 133.9 ms against the recorded
130.6 ms. Between 10⁸ and 4×10⁸ the per-row cost changes by 0.94× to 1.05× per decade at the three
coverages, inside the 1.3× threshold. Below 10⁸ the per-row cost is not constant: the 10⁶ point
sits in cache and costs 0.5× to 0.75× as much per row as 10⁸, and the 10⁷ point at 25 % costs
1.45× as much per row as 10⁸ because its bucket vectors reallocate on every call (explained under
(a) below). A shard at 2³⁰ rows is in the linear regime, so its projection costs 1/N of a
permutation of N such shards. Modelled from the fit, a 2³⁰-row shard costs 1.39 s at 25 %,
0.66 s at 10 % and 0.24 s at 1 %.

**(b) Eight leaf projections per token cost 1.10× the memory of one and 1.40× the build time;
counting the emulation's mask split, 1.53×.** At 10⁴ tokens over a 10⁶-row universe at 1 %, the
one-permutation shape holds 10⁴ bitmaps in 211 MB of resident set (21.1 KB per token) and the
eight-shard shape holds 8×10⁴ bitmaps in 232 MB (23.2 KB per token). The bitmaps' own bytes are
the same to 0.3 %. Build time per token is 153 µs against 213 µs through `project`, and 140 µs
against 208 µs through `project_with` with one reused scratch. The memory ratio is under the 1.5×
threshold; the build-time ratio is under it without the split and over it with the split. At 10⁸
rows split eight ways with one mask, the sharded projection takes 0.78× to 0.90× the time of the
whole one at 10 % and 25 %, and 1.04× at 1 % through `project_with`; the results occupy the same
bytes to 0.13 %.

## The question

Splitting a corpus into N epoch shards inside one process gives each session N row projections,
one per shard, instead of one. The saving depends on `Permutation::project` being linear in the
permutation's size, so that a shard at 2³⁰ rows costs about 1/N of the corpus figure. The cost
depends on how much more N small projections per token take than one large one, in time and in
memory, since a session holds them for its lifetime.

The corpus figure is 1,277 ms for `Permutation::project` at 10⁹ rows. It was measured
single-threaded, median of 3, over a 25 % grant (2.5×10⁸ entities chosen uniformly at random) and
a scattered permutation (a cycle-walked Feistel bijection, uncorrelated with entity order), on this
host with nothing else running (`../2026-08-14-project-decomposition/1e9-scattered.txt`, the line
`Permutation::project  median 1277.3 ms`). The same run gives 130.6 ms at 10⁸ under the same
conditions. Both are wall clock. The engine cites the 10⁹ figure at `RowProjection`'s
`boundary_seg_id` field in `crates/tessera-engine/src/compose.rs` ("one string comparison against
a measured 1 277 ms rebuild") and at `Engine::full_projection_builds` in `session.rs` ("paying
`Permutation::project`, a measured 1 277 ms at 10⁹, on the steady-state path"). `permutation.rs`'s
own cost note adds that the figure was measured against the flat slot array the current paged
format replaced, and that a dense view's page walk is the same reads plus one directory lookup per
2¹⁶ entities.

## Method

Each configuration writes a fresh permutation file through `PermutationWriter` (sequentially:
slot `e` gets the shuffle's image of `e`), opens it with `Permutation::load`, checks it with
`validate_rows`, and projects through the public `Permutation::project`, unchanged. Masks are
entity-space bitmaps. A scattered mask puts each entity in independently with probability
`coverage`. A contiguous mask is one run of `coverage × rows` entities at a random position,
run-optimised. One untimed warm-up call, then five timed calls; the median is the figure. Around
every timed call the harness reads the process's CPU time, page faults and context switches, and
four system-wide `/proc/vmstat` counters, because the box was shared and a wall-clock sample can
belong to the scheduler.

- **(a) Linearity.** Scattered permutations at 10⁶, 10⁷, 10⁸ and 4×10⁸ rows (the 4×10⁸ file is
  1.6 GB); scattered masks at 25 %, 10 % and 1 %. The 25 % arm exists so the fit can be compared
  with the recorded figure under its own conditions; 10 % and 1 % are the design's. Each size ran
  in its own process, twice, in separate passes; the lower median is used and the other is shown.
  At 10⁸ the contiguous mask ran beside the scattered one. One extra size, 3×2²² rows, separates
  the bucket reallocation at 10⁷ from the size itself.
- **(b) One mask, one permutation against eight.** 10⁸ rows as one permutation and as eight of
  1.25×10⁷ with entity ranges split contiguously; the same mask projected both ways. The public
  `Permutation` has no entity offset, so a shard's permutation covers local ids and the global
  mask is restricted to the shard's range and shifted down (`and` with a range, then `add_offset`)
  before projecting. A shard-aware permutation would seek into the mask as `SegmentExtent::project`
  does and pay none of that, so the split is timed apart and shown beside the projection. Both
  entry points are measured: `project`, which the session path uses and which allocates its
  scratch on every call, and `project_with` over one reused `ProjectScratch`, which the artifact
  pass uses.
- **(b) Per token.** 10⁴ tokens' leaf projections over a 10⁶-row universe at 1 %, each token a
  distinct mask, all held at once: 10⁴ bitmaps in the one-permutation shape against 8×10⁴ in the
  eight-shard shape. One shape and one entry point per process. Resident set is `VmRSS` from
  `/proc/self/status`, read after a discarded warm-up token and `malloc_trim`, and again after the
  loop. Beside it: the allocator's own in-use bytes (`mallinfo2`, arena in use plus mmapped
  chunks), croaring's portable serialised size, its `statistics()` container bytes, and
  `size_of::<Bitmap>()` (40 bytes) per bitmap. The last three do not count croaring's per-container
  index arrays or allocator rounding; the resident-set delta does.

## (a) Linearity in rows

`Permutation::project`, scattered permutation, scattered mask, median of 5 after a warm-up. Each
size ran in two processes; the lower median is used and the other is shown. `preempted` is the
used process's non-voluntary context switches summed over its five timed calls; `sleeps` its
voluntary ones. Major faults were zero throughout.

| rows | coverage | projected rows | ms used | cpu ms | min ms | other process ms | preempted / sleeps | ns per row | ns per projected row |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 10⁶ | 25% | 249,658 | 0.97 | 1.00 | 0.95 | 0.98 | 0 / 0 | 0.973 | 3.90 |
| 10⁷ | 25% | 2,501,269 | 19.38 | 19.75 | 18.72 | 19.43 | 0 / 0 | 1.938 | 7.75 |
| 3×2²² | 25% | 3,145,596 | 22.22 | 22.64 | 17.87 | — | 0 / 0 | 1.766 | 7.06 |
| 10⁸ | 25% | 24,995,841 | 133.89 | 136.35 | 129.35 | 134.39 | 2 / 0 | 1.339 | 5.36 |
| 4×10⁸ | 25% | 99,995,995 | 520.18 | 529.54 | 505.50 | 525.55 | 3 / 0 | 1.300 | 5.20 |
| 10⁶ | 10% | 99,907 | 0.44 | 0.46 | 0.44 | 0.45 | 0 / 0 | 0.445 | 4.45 |
| 10⁷ | 10% | 998,943 | 5.95 | 6.07 | 5.61 | 6.55 | 0 / 0 | 0.595 | 5.96 |
| 10⁸ | 10% | 10,001,670 | 63.26 | 64.40 | 62.31 | 65.09 | 0 / 0 | 0.633 | 6.32 |
| 4×10⁸ | 10% | 39,997,554 | 244.45 | 248.85 | 243.15 | 249.61 | 2 / 0 | 0.611 | 6.11 |
| 10⁶ | 1% | 9,883 | 0.11 | 0.12 | 0.10 | 0.11 | 0 / 0 | 0.108 | 10.97 |
| 10⁷ | 1% | 99,547 | 1.38 | 1.42 | 1.20 | 1.56 | 0 / 0 | 0.138 | 13.90 |
| 10⁸ | 1% | 999,345 | 21.33 | 21.73 | 21.30 | 21.61 | 1 / 0 | 0.213 | 21.34 |
| 4×10⁸ | 1% | 4,001,833 | 87.83 | 89.41 | 86.43 | 88.68 | 1 / 0 | 0.220 | 21.95 |

The per-decade factor is `(t₂/t₁)/(n₂/n₁)` normalised to a tenfold step; 1.0 is linear. The fit
is least squares over the sizes at or above 10⁷, including 3×2²².

| coverage | 10⁶→10⁷ | 10⁷→10⁸ | 10⁸→4×10⁸ | fit: a + b·n (ms, n in 10⁶ rows) | r² | predicted at 10⁹ | 4×10⁸ × 2.5 |
|---:|---:|---:|---:|---|---:|---:|---:|
| 25% | 1.99 | 0.69 | 0.95 | 6.0 + 1.285·n | 1.0000 | 1,291 ms | 1,300 ms |
| 10% | 1.34 | 1.06 | 0.94 | 0.9 + 0.610·n | 0.9999 | 611 ms | 611 ms |
| 1% | 1.28 | 1.54 | 1.05 | −0.8 + 0.222·n | 1.0000 | 221 ms | 220 ms |

The recorded 10⁹ figure over a 25 % grant is 1,277 ms. The fit predicts 1,291 ms (1.01×) and the
4×10⁸ point scaled by 2.5 gives 1,300 ms (1.02×). The recorded 10⁸ figure is 130.6 ms; this run's
is 133.9 ms (1.03×), with the ingest running beside it.

Three steps in the table are not linear, and none of them is a property of the sizes a shard would
have.

- **10⁶ is in cache.** The 4 MB slot array, the 125 KB result and the buckets fit in L2 and L3, so
  every lookup and stamp is a cache hit: 0.97 ns per row at 25 % against 1.34 at 10⁸.
- **10⁷ at 25 % pays a bucket reallocation on every call.** 10⁷ rows is 2.38 buckets of 2²². The
  implementation reserves each bucket at the mean plus a quarter (`cardinality / buckets × 5/4`),
  and with a partial third bucket the two full ones hold 26 % more than the mean, exceed the
  reservation and reallocate: 3,432 minor faults on every timed call in both processes, against
  484 to 9,127 on the later calls at 4×10⁸, where the allocator reuses the freed buckets. The
  3×2²² run has no partial bucket. Its five samples split by whether glibc served the bucket
  vectors from fresh mappings: 17.9 to 18.7 ms with 3 to 556 faults (1.42 to 1.49 ns per row, in
  line with 10⁸), and 35.0 to 35.3 ms with about 4,900. That spread puts one minor fault at 1.6 to
  3.6 µs on this guest across the two runs that showed it (derived), and 3,432 of them at 10⁷ is
  5 to 12 ms of its 19.4 ms. The reservation exceeds its slack only when the last bucket is under
  about 40 % full and there are at most three buckets, so permutations between 4×10⁶ and
  1.7×10⁷ rows; a shard at 2³⁰ rows has 256 whole buckets.
- **1 % between 10⁷ and 10⁸ crosses the L3.** At 1 % coverage one slot in a hundred is read, so
  the slot walk costs per page and per cache line rather than per row. The 40 MB array at 10⁷
  still mostly fits the 32 MB L3; the 400 MB one at 10⁸ does not, and from 10⁸ upward the rate is
  flat (0.213 then 0.220 ns per row).

Nothing in the algorithm depends on the permutation's total size except the bucket count, and the
bucket is a fixed 2²² rows, so from 10⁸ upward the per-row cost has no size term left to grow.

### A contiguous entity set

The same 10⁸ permutation with the mask as one run of entities:

| rows | coverage | scattered ms | contiguous ms | contiguous / scattered |
|---:|---:|---:|---:|---:|
| 10⁸ | 25% | 133.89 | 93.22 | 0.70 |
| 10⁸ | 10% | 63.26 | 39.78 | 0.63 |
| 10⁸ | 1% | 21.33 | 10.67 | 0.50 |

A contiguous set reads only the pages it lands in, so a 10 % run reads a tenth of the slot array
where a scattered 10 % mask reads all of it. The rows still scatter across row space, so the bucket
and stamp passes are unchanged; what remains is 3.7 to 4.0 ns per projected row at 25 % and 10 %,
and 10.7 ns at 1 %. A real fragment is signature-sorted and partly contiguous, so it sits between
the two columns; the scattered column is the bound the design should use.

## (b) One permutation against eight

One mask over 10⁸ rows, projected through one permutation of 10⁸ and through eight of 1.25×10⁷,
median of 5. The split is the emulation's cost of restricting the global mask to a shard and is
shown so it can be subtracted; a shard-aware permutation would not pay it.

| entry point | coverage | one: ms (cpu) | sharded: ms (cpu) | split ms | sharded / one | with split | one: portable bytes | sharded: portable bytes | bytes ratio | containers one / sharded |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| project | 25% | 137.04 (139.54) | 121.93 (124.15) | 2.82 | 0.890 | 0.910 | 12,513,208 | 12,529,664 | 1.0013 | 1,526 / 1,528 |
| project | 10% | 76.73 (78.14) | 59.84 (60.94) | 2.92 | 0.780 | 0.818 | 12,513,208 | 12,529,664 | 1.0013 | 1,526 / 1,528 |
| project | 1% | 21.84 (22.25) | 34.62 (35.26) | 1.39 | 1.585 | 1.649 | 2,016,990 | 2,017,062 | 1.0000 | 1,526 / 1,528 |
| project_with | 25% | 133.58 (135.99) | 120.35 (122.54) | 2.77 | 0.901 | 0.922 | 12,513,208 | 12,529,664 | 1.0013 | 1,526 / 1,528 |
| project_with | 10% | 65.02 (66.21) | 57.94 (58.99) | 2.86 | 0.891 | 0.935 | 12,513,208 | 12,529,664 | 1.0013 | 1,526 / 1,528 |
| project_with | 1% | 21.81 (22.24) | 22.72 (23.12) | 1.20 | 1.042 | 1.097 | 2,016,990 | 2,017,062 | 1.0000 | 1,526 / 1,528 |

At 10 % and 25 % the eight projections together take less time than the one: each shard's buckets
and result are an eighth of the size and stay closer to the cache. At 1 % the work per call is
small enough for the per-call cost to show: 1.04× through `project_with`. The `project` cell at
1 % is reported as measured; its five samples were 34.6, 34.8, 34.7, 26.0 and 23.0 ms with no page
faults and no preemption, and the adjacent `project_with` process measured the same configuration
at 22.7 ms, so the 1.585 is a transient of the shared box, not a property of the arm. The results
hold the same rows in the same containers (1,526 against 1,528: two shard boundaries fall inside a
container), so the sharded form costs 16 KB more over 12.5 MB.

## (b) Eight leaves per token, held

10⁴ tokens' leaf projections over a 10⁶-row universe at 1 %, held all at once.

| entry point | shape | bitmaps held | project µs/token (cpu) | split µs/token | RSS Δ MB | RSS Δ KB/token | RSS Δ after trim MB | malloc in-use Δ KB/token | portable KB/token | container KB/token | containers/token |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| project | one (N=1) | 10,000 | 152.9 (168.5) | 0.0 | 211.3 | 21.13 | 210.4 | 21.04 | 20.14 | 20.00 | 16.0 |
| project | sharded (N=8) | 80,000 | 213.4 (231.0) | 20.8 | 232.0 | 23.20 | 231.1 | 21.45 | 20.19 | 20.00 | 16.0 |
| project_with | one (N=1) | 10,000 | 140.3 (155.6) | 0.0 | 211.2 | 21.12 | 210.4 | 21.03 | 20.14 | 20.00 | 16.0 |
| project_with | sharded (N=8) | 80,000 | 207.6 (225.3) | 21.0 | 229.5 | 22.95 | 228.7 | 21.44 | 20.19 | 20.00 | 16.0 |

Sharded over one, per token: build time 1.40× through `project` and 1.48× through `project_with`
(1.53× and 1.63× with the split); resident set 1.10× and 1.09×; allocator in-use bytes 1.02×;
portable bytes 1.003×.

**Memory.** A token's 10⁴ rows land in 16 containers in both shapes (a 10⁶-row space is 16
containers; each 1.25×10⁵-row shard is 2), all array containers of about 625 rows, so the container
bytes are identical (20.0 KB per token) and the portable bytes differ by the extra container
headers (20.14 against 20.19 KB). The eight-shard shape adds seven `Bitmap` structs (280 bytes) and
seven container-index arrays per token; the allocator counts 0.41 KB more in use, and the resident
set grows by 2.07 KB, the remaining 1.66 KB being heap pages the allocator holds for many small
allocations without counting them in use. At 10⁴ tokens that is 21 MB more over 211 MB.

**Build time.** Eight calls per token cost 60 µs more than one through `project` and 67 µs more
through `project_with`, so the size-independent part of one call is about 9 to 10 µs (derived).
Both entry points clear the full 512 KB stamp on every call (`stamp.clear()` then
`stamp.resize(2²²/64, 0)` in `project_with`, which `project` calls with a fresh scratch), and at
1.25×10⁵ rows per shard the shard's rows can touch at most 16 KB of it. Clearing 512 KB at cache
speed is of the order of 10 µs (modelled), which accounts for most of the fixed part. The
difference between the entry points, 13 µs per token at N=1 and 6 µs at N=8, is the allocation
`project` makes for its scratch. The per-token totals of 1.5 to 2.1 s over 10⁴ tokens are what a
server pays once per session; the per-token figure that scales with N is the 9 to 10 µs per extra
call.

## Caveats

- **The box was shared.** The rung 5 ingest ran throughout at nice 10 on one core with about
  10 GB resident and streaming writes, and its builds ran at nice 0 on every core at intervals. A
  step that runs during a build measures the scheduler: one such run showed 1,455 ms of wall clock
  for 791 ms of CPU at 4×10⁸ over 25 %, with no page faults. `run.sh` therefore waits for no
  compiler and a one-minute load under 5 before each step (the final sequence ran at loads of 1.7
  to 2.6), every size ran twice, and every sample records its preemptions, sleeps and faults. The
  figures used had 0 to 3 preemptions across five calls.
- **CPU time is a preemption detector, not a second clock.** `getrusage` CPU time runs 2 to 10 %
  above wall clock on this kernel when nothing preempts the process, so it is shown to expose the
  cases where wall clock is far above it.
- **Minor faults cost 1.6 to 3.6 µs on this guest** (derived above). Any call that receives fresh
  pages from the allocator pays them: the first timed call after a fixture at 4×10⁸ over 25 % took
  674 ms with 110,157 faults against 505 to 535 ms after, and every call at 10⁷ over 25 % takes
  3,432. The medians used exclude the former and include the latter.
- **Memory bandwidth and the L3 were shared throughout**, so figures at the sizes whose working
  set leaves the cache are an upper bound on the idle-box cost. The recorded 10⁹ figure was taken on an idle
  box; the 10⁸ figure here is 3 % above its recorded counterpart.
- **The fixture is synthetic.** A scattered bijection and a uniformly random mask are the shape
  `permutation.rs` designs against and the shape the recorded figure used. The contiguous arm
  bounds a real fragment from the other side.
- **The permutation format is paged.** The recorded figure was measured against the flat slot
  array the paged format replaced. The 10⁸ figure here matches its recorded counterpart within
  3 %, so the directory lookup per page costs nothing visible at these densities.
- **The split is the emulation's, not the design's.** Its cost is 1.2 to 2.9 ms at 10⁸ and 21 µs
  per token at 10⁶, and is reported so it can be subtracted.

## Reproducing

```bash
CARGO_BUILD_JOBS=3 cargo build --release -p tessera-bench --bin epoch_shard_projection
bash probes/2026-09-04-epoch-shard-projection/run.sh all      # about 12 min on a quiet box
python3 probes/2026-09-04-epoch-shard-projection/collate.py   # result.json and the tables
```

Build into the checkout's own target directory: a target shared between checkouts at different
revisions overwrites one checkout's crate metadata with the other's. The 4×10⁸ step needs 1.6 GB
of scratch disk and peaks under 3 GB resident; the whole sequence stays under 4 GB.
