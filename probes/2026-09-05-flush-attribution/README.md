# The flush's stages: the laps, and their proof on medcpt-1m

**Status:** Evidence — measurement, never normative. WSL2, 12 cores, 47 GB, local NVMe. Read the
shapes, not the milliseconds; this box's run-to-run bar is ~30%
([`ingest-rate.md`](../../docs/evidence/memos/2026-08-05-ingest-rate.md), Results).

Commissioned by the campaign handover's §3: after the executor fixes the term binding rung 4's
ingest is the flush (25 publications over 8.83M rows into a 91.9M base, `B/W ≈ 38` against the
row trigger's 4), and nothing lapped the flush's own stages. This probe adds the laps, proves
they partition the flush's wall clock, and runs them on `medcpt-1m`. **The 36M and 92M cells are
not run here**; the binding term at a large base is not named by this document.

## Result on medcpt-1m, f = 0.10

A 900,000-row base, a 100,000-row hold-out through `/control/ingest` at C=8 in 10,000-row
batches, the row trigger at its default 40,000, `--stop-after-ingest`. The ingest phase took
0.6 s at 170,000 rows/s; a flush at this base takes longer than that, so neither of the two
flushes the trigger asked for had published when the phase ended, and both landed in the 2.0 s
drain the probe waits for. Two executions, two publications, 100,000 rows in each count. Another
track's server was idle-serving on this box during the run; two earlier runs on a quiet box read
within 12% of every figure below.

Milliseconds per flush and microseconds per row, differenced across the phase from
`/control/status`'s `write_executor.flush_stages` under `bench-timing`. Per row divides a pool
stage by the rows executed and a publication stage by the rows published; the tick's two stages
divide by executions.

| stage | thread | ms / flush | µs / row |
|---|---|---|---|
| `promote` | pool | 8.7 | 0.18 |
| `rows` | pool | 11.9 | 0.24 |
| `segment` | pool | 34.9 | 0.70 |
| `delta_tier` | pool | 1.4 | 0.03 |
| `filter_extents` | pool | 32.8 | 0.66 |
| `entity_terms` | pool | 1.6 | 0.03 |
| `scoped_extents` | pool | 0.0 | 0.00 |
| `record_extent` | pool | 55.7 | 1.11 |
| **`text_extents`** | pool | **426.5** | **8.53** |
| `digests` | pool | 5.2 | 0.10 |
| `reopen` | pool | 0.1 | 0.00 |
| `shapes` | pool | 0.0 | 0.00 |
| `drop_plan` | pool | 19.9 | 0.40 |
| `failed` | pool | 0.0 | 0.00 |
| **execute sum** | pool | **598.7** | **11.98** |
| `pool_wall` | pool | 598.7 | 11.98 |
| *unattributed* | pool | 0.005 | 0.00 |
| `plan` | executor, at the tick | 50.4 | 1.01 |
| `dispatch` | executor, at the tick | 1.3 | 0.03 |
| `compose` | executor | 8.1 | 0.16 |
| `manifest` | executor | 0.0 | 0.00 |
| `manifest_commit` | executor | 3.7 | 0.07 |
| `with_segment` | executor | 0.1 | 0.00 |
| `shapes_install` | executor | 0.0 | 0.00 |
| `buffer_rebase` | executor | 12.1 | 0.24 |
| `denied` | executor | 0.0 | 0.00 |
| `artifacts` | executor | 0.0 | 0.00 |
| `swap` | executor | 0.0 | 0.00 |
| `rotate` | executor | 9.9 | 0.20 |
| **`drop_superseded`** | executor | **43.7** | **0.88** |
| `discarded` | executor | 0.0 | 0.00 |
| **publish sum** | executor | **77.7** | **1.55** |
| `publish_wall` | executor | 77.7 | 1.55 |
| *unattributed* | executor | 0.02 | 0.00 |

**The laps partition both walls.** The pool's fourteen stages sum to within 5 µs of
`execute_flush`'s 599 ms; the executor's twelve sum to within 23 µs of `publish_flush`'s 78 ms.
A failed execution or a discarded publication charges its tail to `failed` or `discarded`, so the
partition holds whichever way a flush ends; both were zero here.
The residue is the clock reads. The first run of this probe left 22 ms and 39 ms per flush
unattributed (3.7% and 50% of their walls), and both were the locals dropping at the return: the plan's 50,000 buffered items
on the pool, and on the executor the superseded generation, whose buffer holds every row buffered
before the swap. Both are now stages. The partition is also asserted by
`crates/tessera-engine/tests/flush_attribution.rs` on a fixture.

## What the figures say at this base, and what they do not

At a 900,000-row base a flush of 50,000 rows costs **599 ms on the pool and 128 ms on the
executor** (50 ms to plan at the tick, 78 ms to publish). Nothing here is a large-base term yet:

- **`text_extents` is 71% of the pool's time**, 8.5 µs per row. MedCPT declares an indexed text
  column, and this is its analyser and the layer's dictionary, postings and presence for the
  batch. It is work per flushed row, not per base row, and the 36M cell will show whether it
  stays at 8.5.
- **The executor pays O(B) three times per flush**, where `B` is the buffer's occupancy: the
  plan clones every buffered item of the view and sorts them (`plan`, 1.01 µs per row), the
  rebase clones the buffer minus the consumed ids (`buffer_rebase`, 0.24), and the swap frees
  the superseded buffer (`drop_superseded`, 0.88; it lands on this thread when no request still
  holds the old generation, and on that request's thread otherwise). The plan's items are freed
  again on the pool (`drop_plan`, 0.40). Those are per row published. Per item they are closer:
  over the two flushes the clone copied 50,000 items (100,000 buffered minus 50,000 consumed,
  then none) and the drop freed 150,000, so 0.48 µs per item copied against 0.58 per item freed.
- The **plan takes every buffered row of the view**, not `flush_max_items` of them. When a flush
  is slower than the trigger's period the next plan is `B`-sized, which is the `B/W ≈ 38` the
  executor probe recorded at rung 4. That the executor's O(B) terms matter there is inferred
  from this, not measured on this cell.
- `rotate` (9.9 ms) is the overlay snapshot and a new WAL member; `compose` (8.1 ms) opens the
  flush's extents onto the live columns; `manifest_commit` (3.7 ms) is the side-manifest write
  and its fsyncs. Each is a candidate to grow with the base or the overlay, and none can be
  ranked from a 900,000-row base.

## What is left

The 36M cell (`data/ladder/medcpt`, `--fraction 0.10 --concurrency 8 --stop-after-ingest
--reuse-base --copy-base`, a `bench-timing` release binary, the same driver flags as the
executor probe) with the row trigger in force, then the 92M cell once the driver's hold-out read
is bounded. Its reading is: which stages grew from the per-row figures above, by how much, and
whether the growth is in `B` (the three executor terms and `drop_plan`) or in the base
(`compose`, `segment`, `manifest_commit`, `rotate`). The binding term is to be named with a
number there, and a fix or a memo follows, in that order.

## Method

```bash
cargo build --release -p tessera-cli --features tessera-server/bench-timing
python3 probes/2026-09-05-flush-attribution/flush_attribution.py \
  --rung-dir data/ladder/medcpt-1m --work <scratch> --binary <absolute path to the binary> \
  --out probes/2026-09-05-flush-attribution/runs/<cell>.json \
  --fraction 0.10 --concurrency 8 --port0 8171 --stop-after-ingest --reuse-base --copy-base
```

The script runs `test_corpora.common.ingest_cycle` unchanged and reads `/control/status`'s
`write_executor` block before and after the ingest phase. After the phase it waits for the
flushes the phase triggered to land: no flush in flight (`write_executor.flush.in_flight`), every
pool execution published, and the buffer still across two polls half a second apart. The row
trigger asks again at each publication while the buffer holds `flush_max_items` or more, so a
still buffer with nothing in flight is the trigger with nothing more to ask. `drain_s` in the
result is how long that took; on this cell every flush landed in it, and on a large cell most will
land during the ingest. `--print <result.json>` re-prints a finished run's table.

`executions` counts `execute_flush` returns on the pool, `Ok` or `Err`; `flushes` counts
publications that swapped. The pool commits its laps only when it returns, so a flush still
running when a status is read is in neither the stage totals nor the counts. A binary built
without `bench-timing` reports every stage as zero and `bench_timing: false`.

## The instrumentation

`FlushStage` (`crates/tessera-engine/src/flush.rs`) names the stages; `/control/status` reports
them under `write_executor.flush_stages` as two maps, `executor_nanos` and `pool_nanos`, beside
`executions`, `rows_executed`, `flushes` and `rows_published`. They are not added to
`write_executor.stage_nanos`: the pool's time is wall clock on another thread, and the executor
probe's partition (executor stages plus queueing equals submit-to-receipt) holds only while those
laps stay the executor's own. Without the feature the marks read no clock and the call sites are
unchanged, as `WriteStage`'s are.

Raw results: [`runs/`](runs/).
