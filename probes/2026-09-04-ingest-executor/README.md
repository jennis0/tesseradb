# The ingest executor at 36M rows: what the laps convict, and what fixing it is worth

**Status:** Evidence — measurement, never normative. WSL2, 12 cores, 47 GB, local NVMe. Read the
shapes, not the milliseconds; this box's run-to-run bar is ~30%
([`ingest-rate.md`](../../docs/evidence/memos/2026-08-05-ingest-rate.md), Results).

Commissioned because a real rung ingested at **11k rows/s** where the engine's own bench arm
measured **250,000–465,000**, and nothing in the tree said which term the difference was in. The
answer is one term, it is not any of the four the hypotheses ranked first, and it is fixed.

## Result

**MedCPT, 35,920,666 rows, the 10% cell**: a 32,328,599-row base built once, its 3,592,067-row
hold-out through `/control/ingest` at C=8 in 10,000-row batches, `--stop-after-ingest`. Same base
bundle for all three, served as a copy so no cell changes the next cell's starting point.

| | rows/s | ack p50 | ack p99 | 429s | executor µs/row |
|---|---|---|---|---|---|
| before | 11,813 | 1.76 s | 3.90 s | 357 | 21.63 |
| + the flush's row trigger | 50,090 | 1.60 s | 2.45 s | 0 | 13.51 |
| + the buffer's row list behind an `Arc` | **61,999** | **1.27 s** | 2.85 s | **0** | **7.61** |

**5.25×, and the backpressure is gone.** The before column is the campaign's own figure
reproduced — `measurements.json`'s 36M cells read 11,267 and 10,746 rows/s, and the live rung 5
server read 11k at 116.5M rows — so this is the deployment regime, not a fixture.

## The laps at 36M, before the fix

µs per accepted row, differenced across the ingest phase, from `/control/status`'s
`write_executor.stage_nanos` under `bench-timing`. 360 windows, 9,978 rows a window. Indented
rows are sub-laps of `apply_rows`.

| stage | before | + row trigger | + `Arc` list |
|---|---|---|---|
| **`buffer_clone`** | **14.339** | 5.090 | 2.111 |
| `apply_rows` | 2.360 | 2.806 | 2.493 |
| &nbsp;&nbsp;`.buf_insert` | 1.249 | 1.480 | 1.257 |
| &nbsp;&nbsp;`.est_fwd` | 0.628 | 0.682 | 0.648 |
| &nbsp;&nbsp;`.est_inv` | 0.410 | 0.414 | 0.381 |
| &nbsp;&nbsp;`.wal_pos` | 0.036 | 0.036 | 0.038 |
| `allocate` | 2.003 | 1.856 | 0.784 |
| `admit` | 1.311 | 1.935 | 0.527 |
| `wal_fsync` | 0.877 | 1.029 | 1.046 |
| `wal_append` | 0.734 | 0.739 | 0.637 |
| `record_batch` | 0.001 | 0.019 | 0.006 |
| `swap` | 0.001 | 0.036 | 0.002 |
| **executor sum** | **21.626** | **13.510** | **7.606** |
| `submit→receipt` | 152.542 | 138.086 | 115.001 |
| *of which queueing* | 130.916 | 124.576 | 107.395 |
| `apply_nanos_total` | 17.901 | 9.518 | 5.062 |
| `apply_nanos_max` | 1,988 ms | 606 ms | 560 ms |

**`buffer_clone` is 66% of the executor's per-row cost and the whole of the regression.** Every
other stage is within a factor of two of its synthetic figure; the clone is 14.3 µs/row against
0.325 at the bench arm's deepest cell. Nothing else is a candidate: the two `established` inserts
together are 1.04 µs/row (5%), `record_batch` and `swap` are noise, and `submit→receipt` minus the
executor's own stages is 131 µs/row of *queueing* — which is 8 concurrent callers waiting on one
serialised executor doing 21.6 µs of work per row, i.e. exactly the executor and not a handler,
a decode, a duplicate check or the Python driver.

## Why the clone, and why it did not show before

Two multiplied causes, one per fix.

**`B` was never bounded.** `apply_window` deep-copies the ingest buffer once per commit-window
close, so with `B` rows buffered between publications and a close every `W`, a flush interval pays
`B²/2W` copies. Under the age tick alone `B` is the arrival rate times 90 s. Here it was bounded
only by `ingest_buffer_max_items` — the buffer hit its 1,000,000-row ceiling and the run took 357
`429`s, i.e. the deployment spent the cell in backpressure at `B/W = 100`. `ingest-rate.md` sweeps
`B/W` to 24 and measures the optimum at 4; nothing had ever measured 100 because no knob set it.

**And a clone at `B` was 145 ns an entry, not the ~10 ns a hash-table copy costs.** The
`Arc<BufferedItem>` change (`78759b1`) took the *items* out of the copy; the `Vec` holding each
entity's rows stayed a bare `Vec`, so cloning the map still allocated and copied one `Vec` per
buffered row. At `B` = 1M that is a million mallocs per window close — 143 ms, which is the
1,988 ms worst window's shape and the 39.9 s one rung 5 recorded at its own depth.

`allocate` (2.00 → 0.78) and `admit` (1.31 → 0.53) fall with the `Arc` change and touch none of
its code. That is the allocator-pressure coupling `ingest-rate.md` §5 isolated by an A/B,
reproduced here from the other side: stop freeing a million allocations a window and unrelated
stages get faster.

## The hypotheses that were wrong

Recorded because each was ranked above the answer.

- **The `established` maps are not the term.** `std::collections::HashMap<Vec<u8>, EntityId>`
  with SipHash and a heap key per row, at 3.6M live entries, costs **0.628 µs/row**; its
  `FxHashMap` inverse costs 0.410. Together 5% of the executor. A faster hasher is worth ~7% of
  the executor at best and is not taken here.
- **`apply_nanos_max` is not a rehash.** 1,988 ms at `B` = 1M is 2 µs per buffered row, which is
  the clone's own constant at that depth, not a doubling.
- **`set_wal_pos`'s `make_mut` does not copy.** `.wal_pos` is 0.036 µs/row throughout: the stamp
  lands immediately after the insert, where the item's refcount is one.
- **Not the handler, the driver or the duplicate check.** Queueing tracks the executor's own
  per-row cost times the caller count in all three columns (131 ≈ 8 × 21.6/1.3, 107 ≈ 8 × 7.6).

## The base-size and `B/W` axes

`medcpt-1m`, same driver, before the fix — the control that separates "big corpus" from "deep
buffer". A 900k-row base taking a 100k-row hold-out never fills, and reads the bench arm's
figures; the same corpus ingested whole into an empty base reaches `B/W` = 100 and reads the
deployment's.

| cell | `B/W` reached | rows/s | `buffer_clone` | executor sum |
|---|---|---|---|---|
| 900k base, 100k hold-out | 10 | 168,750 | 0.753 | 3.855 |
| empty base, 1M hold-out | 100 | 56,147 | 9.910 | 16.092 |
| 32.3M base, 3.6M hold-out | 100 (at the 429 bound) | 11,813 | 14.339 | 21.626 |

**`B/W` moves the rate 3×; the base size moves it a further 4.8× at the same `B/W`** — the third
row is the second row's shape over a 32× larger corpus, and the residue is the flush's own cost
and the wider rows, not any stage measured here.

## What is left

The clone is still the largest single lap at 2.111 µs/row (28% of the executor) and it is still
`O(B)`: the fix removed the per-entry allocation, not the per-entry copy. Removing the term
altogether means the buffer becoming an immutable per-window chunk list held by the generation,
so a close is `O(W)` — a bigger change than either of these, needing a design note first, worth
~15% of this cell at the pass-through the run shows. `B` itself is now set by how fast a flush
completes at a 32M-row base rather than by the trigger: the last cell publishes 9 times and ends
with 682,067 rows buffered, so **the flush, not the commit window, is what now bounds this cell**.

## Method

```bash
cargo build --release -p tessera-cli --features tessera-server/bench-timing
python3 -m test_corpora.common.ingest_cycle \
  --rung-dir data/ladder/medcpt --work <scratch> --binary <the bench-timing binary> \
  --out runs/<cell>.json --fraction 0.10 --concurrency 8 --port0 8241 \
  --stop-after-ingest --reuse-base --copy-base
```

`--stop-after-ingest` returns after the ingest phase and its laps, so an attribution cell does not
pay for a publication, a flush and a fold it will not read; the 0091 census is run separately,
without it. `--copy-base` serves a copy of the base bundle, because a cell that flushes writes
side manifests into the bundle it serves and the next cell over the same base would 409 on the
hold-out its predecessor published.

`perf record -t <tid>` was **not** run: `perf` is not installed on this box (`perf_event_paranoid`
is 2, which would have allowed it). The laps are the whole of the attribution here, and they
partition the close's wall clock, so nothing is unaccounted: executor sum plus the flush is the
`work_service_nanos_ewma` the status block reports.

Raw results: [`runs/`](runs/).
