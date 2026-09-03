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
import glob
import json
import re
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


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--scan", action="store_true", help="the one pass over the share, resumable")
    ap.add_argument("--combine", action="store_true", help="shards -> metadata.parquet + summary")
    ap.add_argument("--limit", type=int, help="scan only the first N files (a smoke)")
    args = ap.parse_args()

    every = not (args.scan or args.combine)
    if args.scan or every:
        print(json.dumps(scan(args.limit), indent=2))
    if args.combine or every:
        combine()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
