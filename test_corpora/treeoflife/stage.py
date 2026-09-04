"""One pass over TreeOfLife-200M's 666 files, projecting every column but `emb`.

**What this produces.** `staging/metadata.parquet` — `row` (u32, = `entity_id`, the global row
index in file order), `file` (u16), and the sixteen non-vector columns, dictionary-encoded, one row
group per source file. ~29 B/row against `emb`'s ~1,536 B/row (768 halffloats): tiny beside the
vectors, which is why this is the metadata pass and not the vector track's job (`interface.md`).
Everything after this script — `gbif_join.py`, the vector track's `prepare.py` — reads this file and
never the share's `bioclip-2_float16/` again.

**Two stages, one pass over the share.** `scan()` reads each of the 666 files once, projecting the
sixteen columns plus the footer, and writes a per-file shard under `staging/metadata-parts/` — a
few megabytes each, so an interrupted run loses at most one file's worth of the pass rather than
all of it, the same shape as `medcpt/stage.py`'s per-chunk resumability. `combine()` then runs
entirely off local disk: it concatenates the shards in file order into `staging/metadata.parquet`
(one row group per shard, so a page-index read still lands on file boundaries) and computes the
figures the brief asked for — per-`source_dataset` counts, null rates per rank, distinct publishers,
distinct values per rank — in the same pass, since it already holds every shard's table to write it.

    python -m test_corpora.treeoflife.stage --scan       # the one pass over the share, resumable
    python -m test_corpora.treeoflife.stage --combine     # shards -> metadata.parquet + summary.json

**Row order is asserted, not assumed.** `entity_id` is the interface's contract, so a shard is
written with `row = arange(offset, offset + len(table))` computed from `ROWS_PER_FILE` and the file
index alone — the same off-header arithmetic `medcpt/sources.py` uses for its chunk offsets — and
the file's actual row count is checked against it (350,000 for every file but the last) rather than
trusted from the schema.
"""

from __future__ import annotations

import argparse
import concurrent.futures as cf
import glob
import json
import re
import threading
import time
from pathlib import Path

import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

from . import sources


def shards_dir() -> Path:
    path = sources.staging() / "metadata-parts"
    path.mkdir(exist_ok=True)
    return path


def shard_path(i: int) -> Path:
    return shards_dir() / f"shard_{i:05d}.parquet"


def offset_of(i: int) -> int:
    """The global row offset of file `i`, from the interface's fixed row counts — no header read."""
    return i * sources.ROWS_PER_FILE


def stage_file(i: int, path: Path) -> dict:
    """One file: project, stamp `row`/`file`, write the shard."""
    t0 = time.time()
    table = pq.ParquetFile(path).read(columns=sources.METADATA_COLUMNS, use_threads=True)
    n = table.num_rows
    expected = sources.ROWS_PER_FILE if i < sources.FILE_COUNT - 1 else (
        sources.TOTAL_ROWS - offset_of(sources.FILE_COUNT - 1)
    )
    assert n == expected, f"{path.name}: {n} rows, expected {expected}"

    offset = offset_of(i)
    table = table.append_column(
        "row", pa.array(range(offset, offset + n), pa.uint32())
    ).append_column("file", pa.array([i] * n, pa.uint16()))
    # row, file first — the two columns everything downstream joins on.
    order = ["row", "file"] + sources.METADATA_COLUMNS
    table = table.select(order)

    pq.write_table(
        table,
        shard_path(i),
        compression="zstd",
        use_dictionary=sources.DICTIONARY_COLUMNS,
        row_group_size=n,
    )
    source_counts = {
        v.as_py(): c.as_py()
        for v, c in zip(*pc.value_counts(table["source_dataset"]).flatten())
    }
    return {
        "file": i,
        "name": path.name,
        "rows": n,
        "offset": offset,
        "source_dataset_counts": source_counts,
        "seconds": round(time.time() - t0, 2),
    }


def scan(limit: int | None = None) -> dict:
    """The one pass over the share's 666 files, resumable per file."""
    files = sources.treeoflife_files()
    if limit is not None:
        files = files[:limit]

    ledger_path = shards_dir() / "ledger.jsonl"
    done: dict[int, dict] = {}
    if ledger_path.exists():
        for line in ledger_path.read_text().splitlines():
            if line.strip():
                rec = json.loads(line)
                done[rec["file"]] = rec

    t0 = time.time()
    read_bytes_total = 0
    scanned = 0
    with open(ledger_path, "a") as ledger:
        for i, path in enumerate(files):
            if i in done:
                continue
            stats = stage_file(i, path)
            stats["bytes_on_disk"] = shard_path(i).stat().st_size
            ledger.write(json.dumps(stats) + "\n")
            ledger.flush()
            scanned += 1
            read_bytes_total += path.stat().st_size
            if scanned % 25 == 0 or i == len(files) - 1:
                wall = time.time() - t0
                print(
                    f"{scanned}/{len(files) - len(done)} files this run, "
                    f"{i + 1}/{len(files)} total, {wall:.0f}s elapsed",
                    flush=True,
                )

    wall = time.time() - t0
    return {"files_this_run": scanned, "files_skipped": len(done), "seconds_this_run": round(wall, 1)}


def combine() -> dict:
    """Shards -> `metadata.parquet`, one row group per source file, plus the summary figures."""
    t0 = time.time()
    shards = sorted(
        glob.glob(str(shards_dir() / "shard_*.parquet")),
        key=lambda p: int(re.search(r"shard_(\d+)\.parquet", p).group(1)),
    )
    assert len(shards) == sources.FILE_COUNT, (
        f"{len(shards)} shards, expected {sources.FILE_COUNT} — `--scan` has not finished"
    )

    out_path = sources.staging() / "metadata.parquet"
    writer: pq.ParquetWriter | None = None

    source_counts: dict[str, int] = {}
    rank_nulls = {r: 0 for r in sources.RANKS}
    rank_distinct: dict[str, set[str]] = {r: set() for r in sources.RANKS}
    publishers: set[str] = set()
    total_rows = 0

    for shard in shards:
        table = pq.read_table(shard)
        total_rows += table.num_rows
        if writer is None:
            writer = pq.ParquetWriter(
                out_path, table.schema, compression="zstd", use_dictionary=sources.DICTIONARY_COLUMNS
            )
        writer.write_table(table, row_group_size=table.num_rows)

        for v, c in zip(*pc.value_counts(table["source_dataset"]).flatten()):
            source_counts[v.as_py()] = source_counts.get(v.as_py(), 0) + c.as_py()
        for rank in sources.RANKS:
            col = table[rank]
            rank_nulls[rank] += col.null_count
            rank_distinct[rank].update(v for v in pc.unique(col).to_pylist() if v is not None)
        publishers.update(v for v in pc.unique(table["publisher"]).to_pylist() if v is not None)
        del table

    writer.close()

    summary = {
        "rung": sources.RUNG,
        "rows": total_rows,
        "rows_expected": sources.TOTAL_ROWS,
        "files": len(shards),
        "source_dataset_counts": source_counts,
        "rank_null_counts": rank_nulls,
        "rank_null_rates": {r: round(rank_nulls[r] / total_rows, 4) for r in sources.RANKS},
        "rank_distinct_counts": {r: len(rank_distinct[r]) for r in sources.RANKS},
        "distinct_publishers": len(publishers),
        "bytes_on_disk": out_path.stat().st_size,
        "seconds": round(time.time() - t0, 1),
    }
    (sources.staging() / "metadata.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({k: v for k, v in summary.items() if k != "rank_null_counts"}, indent=2))
    return summary



# ------------------------------------------------------------------------ the fit sample
#
# **346 GB of vectors are never staged whole** (`interface.md`, and the vectors brief): the share
# holds 666 x 516 MB of `emb`, and this box has 217 GB free. So the layout is fitted on a sample
# staged here and every row is placed against that fit index in a second pass that reads the share
# and keeps nothing (`routes.py`).
#
# **The sample is uniform over all 666 files, because the corpus is sorted by taxonomy.** A prefix
# of the files is a prefix of the tree of life; a subset of files is a subset of it. Each file
# contributes its equal share, drawn from **one of its seven row groups**, the group index rotating
# with the file (`i % 7`) so no position in a file is systematically over- or under-represented.
#
# ⊘ **That is one seventh of a pass rather than a whole one, and it is a deliberate trade.** A
# truly uniform draw within a file would touch every data page of every `emb` column chunk — the
# column is 73 MB a row group and a scattered 3,754-row take decodes nearly all of it — so a whole
# pass would cost ~3 hours to sample what a rotating single group samples in ~30 minutes. What is
# given up is intra-file uniformity, over a file spanning a very narrow taxonomic range.

#: Rows read at once out of the chosen row group. The whole group is 50,000 x 768 float16 = 73 MB,
#: which is one read.
FIT_SEED = 0

#: What one file's chosen row group costs off the share — 50,000 x 768 halffloats plus the footer,
#: measured at 76.3 MB on `train-00000`. The rate this pass prints is against this and not against
#: the 516 MB file, six sevenths of which it never reads.
ROW_GROUP_MB = 76.3


def fit_allocation(files: int, total: int) -> list[int]:
    """`total` rows spread evenly over `files`, the remainder taken one apiece by the first files.

    Written out rather than left to a float division so the staged sample is exactly `total` rows
    and the per-file offsets below are exact integers.
    """
    base, extra = divmod(total, files)
    return [base + (1 if i < extra else 0) for i in range(files)]


def stage_fit_file(i: int, path: Path, out_slot: slice, matrix, want: int) -> dict:
    """One file's contribution to the fit sample, normalised into its slot of the memmap.

    Returns the global row ids it took, so `fit-rows.npy` can name the corpus rows the layout was
    fitted on — `prepare.py --sample` draws from exactly these.
    """
    import numpy as np

    t0 = time.time()
    f = pq.ParquetFile(path)
    group = i % f.metadata.num_row_groups
    at = sum(f.metadata.row_group(g).num_rows for g in range(group))
    rows_in_group = f.metadata.row_group(group).num_rows
    assert want <= rows_in_group, f"{path.name}: want {want} of a {rows_in_group}-row group"

    table = f.read_row_group(group, columns=["emb"], use_threads=False)
    values = table.column("emb").combine_chunks()
    block = np.asarray(values.values.to_numpy(zero_copy_only=False), dtype=np.float32).reshape(
        rows_in_group, -1
    )
    pick = np.sort(
        np.random.default_rng(FIT_SEED + i).choice(rows_in_group, want, replace=False)
    )
    taken = block[pick]
    # **L2-normalised on the way in**, so cosine and inner product agree and the placement pass
    # normalises the same way. BioCLIP-2 publishes unnormalised embeddings (norms 35.6 … 69.0 over
    # the first row group, measured 2026-09-03), which is why this is done here and not assumed.
    norms = np.linalg.norm(taken, axis=1, keepdims=True)
    zero = int((norms == 0).sum())
    np.divide(taken, np.maximum(norms, 1e-12), out=taken)
    matrix[out_slot] = taken.astype(np.float16)
    del block, taken, table, values

    offset = offset_of(i) + at
    return {
        "file": i,
        "group": group,
        "rows": want,
        "zero_norm": zero,
        "global_rows": (offset + pick).astype("uint32").tolist(),
        "seconds": round(time.time() - t0, 2),
    }


def stage_fit(total: int = sources.FIT_ROWS, workers: int = 3, limit: int | None = None) -> dict:
    """The fit sample: `staging/fit.f16`, `staging/fit-rows.npy` and `staging/fit.json`.

    Resumable per file through `fit-parts/ledger.jsonl`; the memmap is preallocated and each file
    writes only its own slot, so a rerun fills the holes an interrupted run left. `workers` reader
    threads, because SMB here saturates at two to three concurrent readers (~30 MB/s against 20
    MB/s on one, measured 2026-09-03) and a fourth buys nothing.
    """
    import json as _json

    import numpy as np

    files = sources.treeoflife_files()
    if limit is not None:
        files = files[:limit]
    counts = fit_allocation(len(files), total)
    offsets = np.concatenate([[0], np.cumsum(counts)]).astype(np.int64)

    staging = sources.staging()
    parts = staging / "fit-parts"
    parts.mkdir(exist_ok=True)
    ledger_path = parts / "ledger.jsonl"
    done: dict[int, dict] = {}
    if ledger_path.exists():
        for line in ledger_path.read_text().splitlines():
            if line.strip():
                rec = _json.loads(line)
                done[rec["file"]] = rec

    path = staging / "fit.f16"
    mode = "r+" if path.exists() and path.stat().st_size == total * sources.EMBED_DIM * 2 else "w+"
    matrix = np.memmap(path, dtype=np.float16, mode=mode, shape=(total, sources.EMBED_DIM))

    todo = [i for i in range(len(files)) if i not in done]
    t0 = time.time()
    lock = threading.Lock()
    completed = 0
    with open(ledger_path, "a") as ledger:
        def run(i: int) -> None:
            nonlocal completed
            rec = stage_fit_file(
                i, files[i], slice(int(offsets[i]), int(offsets[i + 1])), matrix, counts[i]
            )
            with lock:
                ledger.write(_json.dumps(rec) + "\n")
                ledger.flush()
                done[i] = rec
                completed += 1
                if completed % 25 == 0 or completed == len(todo):
                    wall = time.time() - t0
                    rate = completed * ROW_GROUP_MB / wall
                    print(
                        f"{completed}/{len(todo)} files this run "
                        f"({len(done)}/{len(files)} total), {wall:.0f}s, ~{rate:.1f} MB/s",
                        flush=True,
                    )

        with cf.ThreadPoolExecutor(workers) as pool:
            for fut in cf.as_completed([pool.submit(run, i) for i in todo]):
                fut.result()

    matrix.flush()
    del matrix
    complete = len(done) == len(files)
    if complete:
        rows = np.concatenate(
            [np.asarray(done[i]["global_rows"], dtype=np.uint32) for i in range(len(files))]
        )
        assert len(rows) == total, f"{len(rows)} staged rows against {total}"
        assert np.all(np.diff(rows) > 0), "the staged row ids are not strictly ascending"
        np.save(staging / "fit-rows.npy", rows)

    summary = {
        "rows": total,
        "dim": sources.EMBED_DIM,
        "seed": FIT_SEED,
        "files_done": len(done),
        "files": len(files),
        "complete": complete,
        "zero_norm": sum(r["zero_norm"] for r in done.values()),
        "bytes_on_disk": path.stat().st_size,
        "seconds_this_run": round(time.time() - t0, 1),
        "share_MBps_this_run": round(len(todo) * ROW_GROUP_MB / max(time.time() - t0, 1e-9), 1)
        if todo
        else None,
    }
    (staging / "fit.json").write_text(_json.dumps(summary, indent=2) + "\n")
    print(_json.dumps(summary, indent=2))
    return summary


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--scan", action="store_true", help="the one pass over the share, resumable")
    ap.add_argument("--combine", action="store_true", help="shards -> metadata.parquet + summary")
    ap.add_argument("--fit", action="store_true",
                    help="the vector track's fit sample: staging/fit.f16, resumable per file")
    ap.add_argument("--fit-rows", type=int, default=sources.FIT_ROWS,
                    help="rows in the fit sample, spread evenly over the 666 files")
    ap.add_argument("--workers", type=int, default=3, help="concurrent share readers for --fit")
    ap.add_argument("--limit", type=int, help="scan only the first N files (a smoke)")
    args = ap.parse_args()

    if args.fit:
        stage_fit(args.fit_rows, workers=args.workers, limit=args.limit)
        return 0

    every = not (args.scan or args.combine)
    if args.scan or every:
        print(json.dumps(scan(args.limit), indent=2))
    if args.combine or every:
        combine()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
