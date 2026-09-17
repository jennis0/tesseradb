# The session row projection's transient memory at rung 6

Status: measured 2026-09-16 on `data/ladder/gbif/bundle` (3,495,729,729 rows, one segment, 834
buckets) and on the 64-part smoke bundle `data/ladder/gbif-64p` rebuilt into a scratch path
(25,846,007 rows). Not normative; re-take a figure before relying on it.

## What was asked

`RowSpace::project` over a principal above 45% coverage held 2.4 to 4.6 GiB of anonymous memory at
rung 6, against a result of 212 to 415 MiB. This directory accounts for that peak and measures the
change to `crates/tessera-store/src/permutation.rs` that removes it.

## The cause

The walk gathers rows into buckets of 2²² rows each and emits them every 16.7 million rows (the
window). Each bucket was its own `Vec<u32>`, and a `Vec` keeps its capacity after it is cleared.
A principal's entities do not reach row space evenly within a window: across the traced walks a
window's rows landed in between 7 and 684 of the 834 buckets, and in a different set of buckets
from one window to the next. Over a walk every bucket grew to the largest share it took in any
window, so the buckets together held gigabytes while no window held more than 64 MiB.

The buckets now fill 4,096-row chunks from one pool, sized before the walk for a window plus one
partly filled chunk per bucket. That is 77 MiB at 834 buckets and at most 82 MiB at the 1,025
buckets of the `u32` entity ceiling (both modelled). The pool grows geometrically between calls
through a reused scratch, capped at that size.

## Accounting, before the change

One traced walk per principal on `main` at `cc770c30` with a per-window trace added (not
committed): the buckets' capacity, their resident pages read with `mincore` at the end of the walk,
glibc's `mallinfo2`, and `RssAnon`. Every figure is measured, in MiB. The result as held is the
changed build's anonymous memory at the end of the same walk, which holds only the result.

| principal | windows | anon rise | bucket pages resident | result as held | sum as share of rise | bucket capacity | bucket growth and result payload as share of malloc growth |
|---|---|---|---|---|---|---|---|
| countries p50 | 105 | 2,397 | 1,899 | 283 | 91.0% | 80 → 2,606 | 99.9% |
| species weighted 1k | 103 | 2,485 | 1,934 | 491 | 97.6% | 80 → 2,521 | 99.6% |
| species weighted 10k | 164 | 3,964 | 3,271 | 506 | 95.3% | 80 → 4,231 | 99.8% |
| species weighted 100k | 188 | 4,489 | 3,707 | 526 | 94.3% | 80 → 4,934 | 99.8% |
| year uniform 300 | 123 | 3,363 | 2,603 | 545 | 93.6% | 80 → 3,560 | 99.9% |

The rest of the rise, 58 to 216 MiB, is taken to be freed bucket buffers the allocator kept after a
`Vec` grew and moved. That is inferred, not separately measured.

Ruled out as the peak, each by the table: the window of row ids (64 MiB against gigabytes), the
result held unoptimised and croaring's growth in the unions (the whole result as held is 283 to
545 MiB), and the per-window part (after the change the peak beyond the result is 68 to 72 MiB).
The fragment is built before the sampler's baseline.

## Before and after

`rung6-before.json` is `main` at `cda12fd8`, whose store, roaring, filter and authorisation crates
are identical to those of `cc770c30`; `rung6-after.json` is the changed build, whose store code is that of `f48c0fb4` before its comments and formatting were settled. Both ran on
the same principals and seed under the same cap, one after the other. Walk time is the median of
three; the peak is the largest of three. Measured.

| principal | coverage | walk before | walk after | anon peak before | anon peak after | after, minus the result as held | result portable size, unoptimised |
|---|---|---|---|---|---|---|---|
| countries p50 | 50.0% | 9.52 s | 6.77 s | 2,413 MiB | 351 MiB | 68 MiB | 212 MiB |
| species weighted 1k | 49.1% | 12.88 s | 10.64 s | 2,486 MiB | 561 MiB | 70 MiB | 397 MiB |
| species weighted 10k | 78.6% | 18.64 s | 13.43 s | 3,968 MiB | 578 MiB | 72 MiB | 407 MiB |
| species weighted 100k | 89.8% | 19.08 s | 14.11 s | 4,501 MiB | 598 MiB | 72 MiB | 412 MiB |
| year uniform 300 | 58.9% | 15.63 s | 12.15 s | 3,352 MiB | 616 MiB | 70 MiB | 415 MiB |

The digest of each result after `run_optimize` is equal between the two builds for every principal.
The walk is faster after the change. The likely reason is that no bucket is reallocated and copied
as it grows; that is inferred, not separately measured. The first walk of each principal after the
open is slower in both builds and is not the median.

## A reused scratch over many small masks

The artifact pass projects each artifact's members through one `ProjectScratch`. `term-images/`
holds the same shape on the smoke bundle: every term of a value column projected through
`project_base_with` with one scratch, in term order and in rising size, three runs of five
repetitions per build. `main` is `cda12fd8`; `exact` replaces the pool with one sized exactly for
each new largest mask; `geometric` is the committed form, which doubles the pool, capped at a
window's pool. CPU is the thread's own clock, in seconds; minor faults are the median over the
repetitions after the first. Measured.

| column | terms | build | order | CPU min | CPU median | minor faults |
|---|---|---|---|---|---|---|
| specieskey | 185,418 | main | term | 0.815 | 0.855 | 1,473 |
| specieskey | | main | rising | 0.871 | 0.900 | 1,546 |
| specieskey | | exact | term | 0.826 | 0.839 | 1,806 |
| specieskey | | exact | rising | 0.869 | 0.897 | 2,034 |
| specieskey | | geometric | term | 0.826 | 0.842 | 1,772 |
| specieskey | | geometric | rising | 0.871 | 0.894 | 1,502 |
| year | 508 | main | term | 0.318 | 0.329 | 7,107 |
| year | | main | rising | 0.320 | 0.329 | 7,444 |
| year | | exact | term | 0.309 | 0.321 | 8,254 |
| year | | exact | rising | 0.316 | 0.327 | 9,887 |
| year | | geometric | term | 0.307 | 0.317 | 6,878 |
| year | | geometric | rising | 0.315 | 0.321 | 7,590 |

Geometric growth removes the extra faults of exact sizing, which are largest in rising order (9,887
against 7,590 for year). CPU differs between the three builds by no more than the spread between
repetitions. At 7 buckets the smoke bundle's pool is small; the fault term exact sizing adds grows
with the pool, so rung 6's 834 buckets would show more of it. That is modelled, not measured.

A whole build of the smoke bundle was also timed with both pool forms. Its `artifact_pass` stage
took 9.4 to 12.3 s over four builds with exact sizing and 9.6 to 10.9 s over three with geometric
growth, within the variation of a shared box, so it does not separate the two.

## Commands

The probe is `crates/tessera-bench/src/bin/project_transient_probe.rs`.

```
cargo build --release -p tessera-bench --bin project_transient_probe

systemd-run --user --scope --collect -p MemoryMax=24G -p MemorySwapMax=2G nice -n 10 \
  target/release/project_transient_probe --bundle data/ladder/gbif/bundle \
  --country p50=AU,ES,GB,HT,LU,SM,US,XZ \
  --dim specieskey=1000:weighted --dim specieskey=10000:weighted \
  --dim specieskey=100000:weighted --dim year=300:uniform \
  --reps 3 --out rung6-after.json

target/release/project_transient_probe --bundle <gbif-64p bundle> \
  --term-images specieskey --reps 5 --out term-images/specieskey-geometric-1.json
```

The before run is the same command with the probe built on `cda12fd8`. The seed is the probe's
default, 20260916.
