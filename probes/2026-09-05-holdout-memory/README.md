# The hold-out read: what held 38 GB, and the reader that holds one row group

**Status:** Evidence, never normative. WSL2, 12 cores, 47 GB, local NVMe, pyarrow 25.0.1
(default pool `mimalloc`; `jemalloc` and `system` available). Rung 4's `points.parquet`: 52 GB of
ZSTD parquet, 102,117,343 rows, 718 row groups of 262,144 rows, ~373 MB decoded each, abstracts
included. The 10% hold-out is 10,211,734 rows, 1,022 bodies of 10,000 rows.

## Result

The driver's `HoldOut.batches` grew by about 130 MB for every row group it read and would have
reached ~50 GB over the whole file. The growth is inside pyarrow's `iter_batches` reader: Arrow's
own live-byte count climbs in step with the resident set, the same under `mimalloc` and `system`,
and a bare `iter_batches` loop that drops every batch grows as much as the driver did. Reading
with `read_row_group` holds one row group; the same file then streams whole in 181 s with the
driver between 1.2 and 2.7 GiB and a 3.09 GiB high-water mark.

| reader | pool | bodies | wall | VmRSS at end | VmHWM | Arrow pool live at end |
|---|---|---|---|---|---|---|
| `iter_batches`, as committed at `7ab26081` | mimalloc | 300 of 1,022 | 65 s | 16.66 GiB | 16.67 GiB | 14.82 GiB |
| `iter_batches`, as committed | system | 300 of 1,022 | 136 s | 15.39 GiB | 15.61 GiB | 14.82 GiB |
| `read_row_group` (the fix) | mimalloc | **1,022 of 1,022** | 181 s | 1.51 GiB | **3.09 GiB** | 0.00 GiB |

Measured. The two `before` runs were stopped at 300 bodies (115 row groups) because the slope was
already the answer and the whole file would have swapped the box; the memo's 38 GB at ~900 bodies
is on the same line. `release_unused()` after every row group was in the committed reader and is
in both `before` rows: it cannot return memory Arrow still counts as live.

### The curves

`before-mimalloc.csv`, `before-system.csv` and `after.csv` carry one sample a second
(`holdout_memory.py` writes them). Every hundredth body or so:

*Before, `mimalloc`* (`before-mimalloc.csv`)

| bodies | t (s) | VmRSS (GiB) | VmHWM (GiB) | Arrow pool live (GiB) |
|---|---|---|---|---|
| 0 | 0 | 0.08 | 0.08 | 0.00 |
| 48 | 20 | 4.52 | 5.13 | 3.20 |
| 102 | 29 | 7.02 | 7.02 | 5.61 |
| 150 | 38 | 9.47 | 9.55 | 8.02 |
| 201 | 47 | 11.74 | 11.74 | 10.24 |
| 250 | 56 | 14.37 | 14.37 | 12.56 |
| 300 | 65 | 16.66 | 16.67 | 14.82 |

*Before, `system`* (`before-system.csv`): the pool column is the same to the byte, so the growth
is Arrow allocations that are still live and not memory an allocator kept after a free.

| bodies | t (s) | VmRSS (GiB) | VmHWM (GiB) | Arrow pool live (GiB) |
|---|---|---|---|---|
| 0 | 0 | 0.07 | 0.07 | 0.00 |
| 99 | 41 | 5.88 | 6.00 | 5.38 |
| 199 | 84 | 10.45 | 10.64 | 10.00 |
| 300 | 136 | 15.39 | 15.61 | 14.82 |

*After* (`after.csv`), the whole file:

| bodies | t (s) | VmRSS (GiB) | VmHWM (GiB) | Arrow pool live (GiB) |
|---|---|---|---|---|
| 0 | 0 | 0.07 | 0.07 | 0.00 |
| 99 | 18 | 1.70 | 3.09 | 0.21 |
| 199 | 33 | 1.19 | 3.09 | 0.36 |
| 399 | 67 | 1.88 | 3.09 | 0.59 |
| 602 | 110 | 1.78 | 3.09 | 0.78 |
| 798 | 146 | 2.17 | 3.09 | 0.60 |
| 999 | 177 | 1.52 | 3.09 | 0.12 |
| 1,022 | 181 | 1.51 | 3.09 | 0.00 |

The high-water mark is set in the first 18 s, while the first row groups are decoded on twelve
threads, and is not approached again.

## Where the memory was: the bisect

Five loops over the same 30 row groups, each printing Arrow's live pool bytes at the end
(`retain_bisect.py`). Every variant drops what it reads.

| variant | what the loop does | Arrow pool live after 30 row groups |
|---|---|---|
| A | `iter_batches(batch_size=2^17)`, each batch dropped at once | 4.44 GiB |
| B | A, then `Table.from_batches` and `filter` by the hold-out, the result dropped | 4.44 GiB |
| C | `read_row_group(i)` and the same filter, both dropped | **0.01 GiB** |
| D | B, then `encode_batch` on the kept rows, the body dropped | 4.44 GiB |
| E | B, with every filtered table deliberately kept (786,548 rows, 1.16 GiB by `nbytes`) | 5.60 GiB |

A alone accounts for the whole of the growth, ~150 MB a row group; the filter, the encoder and the
driver's pending list add nothing. E is the scale: keeping every kept row costs 1.16 GiB, which is
the difference between E and B. The retention is a property of the `RecordBatchReader` that
`ParquetFile.iter_batches` returns in this pyarrow, over the life of the iterator, and this probe
does not say which structure inside it holds the bytes.

## What changed

`HoldOut.batches` and `filter_parquet` in `test_corpora/common/ingest_cycle.py` read with
`read_row_group` and no longer call `release_unused` after each group. `filter_parquet` is the
split, which reads the same file the same way before the base is built; its residue was
attributed to the allocator in a comment and was this.

Not taken: a reader subprocess feeding bodies over a pipe. It would have moved the growth to
another process on the same box, not removed it.

## Method

```bash
# a cached hold-out, so a run measures the reader and not the split
python3 probes/2026-09-05-holdout-memory/holdout_memory.py --rung data/ladder/paperseek \
    --out before-mimalloc.csv --held-cache held-f010.npy --limit-batches 300          # at 7ab26081
python3 probes/2026-09-05-holdout-memory/holdout_memory.py --rung data/ladder/paperseek \
    --out before-system.csv --held-cache held-f010.npy --limit-batches 300 --pool system
for v in A B C D E; do
  python3 probes/2026-09-05-holdout-memory/retain_bisect.py --rung data/ladder/paperseek \
      --held held-f010.npy --variant $v --row-groups 30
done
python3 probes/2026-09-05-holdout-memory/holdout_memory.py --rung data/ladder/paperseek \
    --out after.csv --held-cache held-f010.npy                                         # with the fix
```

The box was otherwise idle (`pgrep -af "tessera (build|serve)"` empty) and no server was loaded:
the consumer discards each body, so the figures are the reader's alone.
