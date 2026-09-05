# The flush's stages: the laps, their proof on medcpt-1m, and the term at 36M

**Status:** Evidence — measurement, never normative. WSL2, 12 cores, 47 GB, local NVMe. Read the
shapes, not the milliseconds; this box's run-to-run bar is ~30%
([`ingest-rate.md`](../../docs/evidence/memos/2026-08-05-ingest-rate.md), Results).

Commissioned by the campaign handover's §3: after the executor fixes the term binding rung 4's
ingest is the flush (25 publications over 8.83M rows into a 91.9M base, `B/W ≈ 38` against the
row trigger's 4), and nothing lapped the flush's own stages. This probe adds the laps, proves
they partition the flush's wall clock, runs them on `medcpt-1m`, and then on the 36M rung, where
the term is named: **the pool flushes at 14.8 µs a row and the ingest arrives at 15.4, so the
flush runs back to back and the row trigger cannot hold `B` down; 69% of the pool's time is the
text index of the flushed rows** (§"Result on MedCPT, 36M"). The 92M cell is not run here.

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

### Inside `text_extents`, medcpt-1m

**Measured 2026-09-05** (`runs/medcpt-1m-f010-text.json`), the same cell and flags, the binary
at `e54ff573` with the six `Text*` sub-laps. The controller's 36M ingest cycle was running on the
box throughout (load average 14.6 on 12 cores), so every figure here is inflated against the
table above: ingest ran at 57,226 rows/s against 170,000, and `text_extents` read 10.3 µs a row
against 8.5. Read the shares. Two executions, two publications, 100,000 rows in each count.

`write_text_extents` runs once per indexed `text` column (MedCPT declares one). Per column it
gathers the column's rows from the plan and creates the layer's directory; then for each row it
runs the analyser over the prose (`Analyser::tokens`: the case fold, the NFKC normalisation and
the word segmenter, returning one `String` per token) and inserts each token into a `BTreeMap`
from term to posting list; then it writes the dictionary from the map's keys, the postings from
its values, and the presence bitmap. The sub-laps are those six, and the two per-row ones read
the clock twice a row.

| sub-stage | ms / flush | µs / row | share of `text_extents` |
|---|---|---|---|
| `text_rows` | 3.9 | 0.08 | 0.8% |
| **`text_tokenise`** | **272.5** | **5.45** | **52.8%** |
| **`text_terms`** | **210.5** | **4.21** | **40.8%** |
| `text_dict` | 5.3 | 0.11 | 1.0% |
| `text_postings` | 22.5 | 0.45 | 4.4% |
| `text_presence` | 0.5 | 0.01 | 0.1% |
| **text sum** | **515.2** | **10.30** | 99.9% |
| `text_extents` | 515.7 | 10.31 | |
| *unattributed* | 0.6 | 0.01 | 0.1% |

**The sub-laps partition `text_extents`**: 0.6 ms a flush is unattributed, which is the
digest-list pushes after the call returns and the 200,000 clock reads. The pool's partition still
closes (5 µs a flush unattributed); the sub-laps are in `pool_nanos` and not in the execute sum.

**94% of the text index is the per-row loop, and none of it is the files.** The analyser is
5.45 µs a row over a ~100-character title, and the term-map insert is 4.21: the three writes
together are 0.57. The map insert is one `String` allocation per token from `Analyser::tokens`
and a `BTreeMap<String, _>` lookup per token; `tessera-analyse` documents that a term seen before
is looked up and its freshly allocated key dropped, and provides `for_each_token`, which yields
borrowed tokens over reused buffers for exactly this loop. The build's text index uses it; the
flush's does not. Not changed here: the stage does what it did, and the sub-laps say what a
change would have to move.

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

## Result on MedCPT, 36M, f = 0.10

**Measured 2026-09-05** on a quiet box (`runs/medcpt-36m-f010.json`): a 32,328,599-row base built
once, its 3,592,067-row hold-out at C=8 in 10,000-row batches, `--stop-after-ingest`, the row
trigger at its default 40,000, the binary at `d4f5813d`. Ingest ran at **64,911 rows/s**
(executor 8.17 µs/row, queueing 95.2), the campaign's own figure for this cell reproduced. **Ten
flushes, 3,560,000 rows executed and published, 356,000 rows a flush** — `B/W ≈ 9` against the
trigger's 4; nine had published when ingest ended with 462,067 rows buffered, the tenth landed in
the 7.0 s drain. Both partitions close (3.0 ms and 0.04 ms a flush unattributed); `failed` and
`discarded` are zero.

| stage | thread | 1M: µs / row | 36M: µs / row | 36M: ms / flush |
|---|---|---|---|---|
| `promote` | pool | 0.18 | 0.19 | 65.9 |
| `rows` | pool | 0.24 | 0.23 | 82.8 |
| `segment` | pool | 0.70 | 0.69 | 244.9 |
| `filter_extents` | pool | 0.66 | 0.91 | 324.3 |
| `record_extent` | pool | 1.11 | 1.66 | 589.5 |
| **`text_extents`** | pool | **8.53** | **10.17** | **3,621.3** |
| `drop_plan` | pool | 0.40 | 0.82 | 292.6 |
| the other six | pool | 0.16 | 0.14 | 51.0 |
| **execute sum** | pool | **11.98** | **14.81** | **5,272.4** |
| `plan` | executor, at the tick | 1.01 | 1.21 | 432.0 |
| `compose` | executor | 0.16 | 0.10 | 37.1 |
| `buffer_rebase` | executor | 0.24 | 0.47 | 168.1 |
| `rotate` | executor | 0.20 | 0.07 | 26.2 |
| `drop_superseded` | executor | 0.88 | 0.12 | 44.3 |
| the other seven | executor | 0.07 | 0.02 | 8.1 |
| **publish sum** | executor | **1.55** | **0.80** | **283.8** |

**Per row, the flush grew a quarter, not thirty-six times.** The pool's cost went from 12.0 to
14.8 µs a row over a 36× larger base, most of it in `text_extents` (+1.6), `record_extent`
(+0.6) and `drop_plan` (+0.4); the executor's publication fell (the superseded buffer's free landed
on a request thread here, not the executor's). No stage in the table is a base-size term:
`compose` is 4.6× per flush and 0.6× per row, `rotate` 2.6× and 0.4×, `manifest_commit` 5.8 ms
against 3.7. What grew per flush is `B`, and what set `B` is the pool's throughput.

**The term, with its number.** The pool executes a flush at **14.81 µs a row**, so its ceiling is
67,500 rows/s; the ingest arrived at 64,911 rows/s, 15.4 µs a row. The pool was therefore busy
96% of the phase, every flush was planned against whatever had arrived during the last one, and
the row trigger — which fires at 40,000 buffered rows — was always already due. `B` is not the
trigger's 40,000 but `rate × T_flush`, and `T_flush` is `14.8 µs × B`: the two are consistent only
with a pool at saturation, which is what was measured. **Of the 14.81 µs, `text_extents` is
10.17 — 69% — and it is a cost per flushed row and not per base row: 8.5 at a 900,000-row base,
10.2 at 32,000,000, a fifth more over 36×.** MedCPT's text column is a title of ~100 characters. Rung 4's is an abstract of
~1,500, and its ingest read 7,030 rows/s with the flush binding
([`../2026-09-04-ingest-executor/`](../2026-09-04-ingest-executor/README.md)): if the text index
costs in proportion to the prose, its pool flushes at ~140 µs a row there, which is 7,100 rows/s.
**That is inferred from two points and one assumption, not measured**; the 92M cell measures it.

**What follows is a memo, not a fix.** The stage is the analyser, the dictionary, the postings
and the presence bitmap for the flushed rows' text (`write_text_extents`, write-path §4.3); it is
not one change. Removing it from the flush's critical path would change when an ingested row
becomes searchable, which is a contract question and the owner's; the sub-laps under §"Inside
`text_extents`" say where its time goes on the 1M cell. Neither is taken here.

## What is left

The 92M cell (`data/ladder/paperseek`, the same flags, after the 91.9M base is rebuilt), which
measures `text_extents` on abstracts and settles the inference above, and the `Text*` sub-laps
read at 36M and 92M, where the prose is longer and the term map larger.

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

The six `text_*` stages in `pool_nanos` partition `text_extents` and are printed indented under
it with their own sum and residue; they are not in the execute sum.

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
