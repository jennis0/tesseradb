"""The GBIF join: which TreeOfLife rows can carry a coordinate.

**What this produces.** `staging/gbif-coordinates.parquet` — `row` (u32, `entity_id`), `lat`, `lon`,
`coordinateuncertaintyinmeters`, `gbif_license`, one row per TreeOfLife row whose `source_dataset`
is `gbif` and whose `source_id` matches a GBIF `gbifid`. Plus `staging/join.json`: rows attempted,
matched, matched-with-coordinates, and the per-`source_dataset` breakdown (`interface.md`).

**The key is a hash, not the number itself, on purpose.** Both `source_id` and `gbifid` are plain
digit strings on this vintage (checked directly, not assumed from the schema), so a `uint64` cast
would work today — but the join is written against the *string* column each side actually declares.
`pandas.util.hash_array` gives a deterministic `uint64` per string (the same value across separate
process runs — checked, since `build_ids` and `scan` are separate invocations and the scan may be
resumed by a third).

**Nothing holds all 222,660,750 `gbif` ids' strings at once.** The first version of `build_ids` did
— `dataset.to_table(...).combine_chunks().to_numpy(...)` — and was killed at 24.8 GB RSS forty
seconds in, on a box where a sibling track already holds ~22 GB and the budget here is ~8 GB.
`build_ids` now reads `metadata.parquet` **one row group at a time** (a row group is one source
file, ≤350,000 rows), hashes that row group's `gbif` strings, writes the `(hash, row)` pair into a
preallocated array, and discards the strings before the next row group — so the only things resident
for the whole pass are two flat numeric arrays, ~2.7 GB total, not a string column. The **wanted set
kept in memory is `(hash, row)` pairs, sorted by hash — no strings** — for the same reason: holding
222M Python string objects for the run's three-hour duration was the other way this would have
stayed over budget even after the build fix.

**One TreeOfLife `source_id` can repeat.** A sample file that is entirely `gbif` (`train-00030`)
has 350,000 rows over only 225,527 distinct `source_id`s — one GBIF occurrence can back several
images. So the id set is not a set at all but a multiset: `build_ids` keeps one `(hash, row)` pair
per TreeOfLife row, sorted by hash, and a GBIF part's `gbifid` can expand to several output rows.
`np.searchsorted` with `side="left"`/`"right"` gives the *range* of the sorted array a hash falls
in rather than one position, which is what the duplicate-`source_id` case needs.

**The match is by hash alone, and the collision risk is small and stated rather than hidden.** With
no wanted-side string resident during the scan, a candidate cannot be checked against the true
`source_id` before it is written — only against the true `gbifid`, which the scanned GBIF part
already holds (kept in the shard for the audit below, dropped from the final columns). The birthday
bound at this cardinality (`README-join.md` §2 carries the arithmetic) is small but not zero, and it
is reported rather than assumed away. `combine()`'s spot check reads the true `source_id` back from
`metadata.parquet` for a hundred sampled matches — a hundred targeted, single-row-group reads, not
a resident copy of the whole column — and confirms hash-only matching agreed with the real strings
on the sample.

**Resumable per part**, the same shape as `medcpt/stage.py` and PaperSeek's `extract.py`: each
GBIF part's matched rows go to their own shard under `staging/gbif-parts/`, and a JSONL ledger
records the parts that finished (and what each one cost) so a rerun skips them.

    python -m test_corpora.treeoflife.gbif_join --ids       # the id set, from metadata.parquet
    python -m test_corpora.treeoflife.gbif_join --scan      # the one scan of GBIF, resumable
    python -m test_corpora.treeoflife.gbif_join --combine   # shards -> gbif-coordinates.parquet
"""

from __future__ import annotations

import argparse
import glob
import json
import re
import resource
import time
from pathlib import Path

import numpy as np
import pandas as pd
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

from . import sources

#: `(hash, row)` only — no strings, deliberately (module docstring).
IDS_SCHEMA = pa.schema([pa.field("hash", pa.uint64()), pa.field("row", pa.uint32())])

#: `gbifid` is carried per matched row for the combine-time audit and dropped before the final
#: output — cheap because it is only ever the *matched* subset of a part, never the whole column.
SHARD_SCHEMA = pa.schema(
    [
        pa.field("row", pa.uint32()),
        pa.field("lat", pa.float64()),
        pa.field("lon", pa.float64()),
        pa.field("coordinateuncertaintyinmeters", pa.float64()),
        pa.field("gbif_license", pa.string()),
        pa.field("gbifid", pa.string()),
    ]
)

OUTPUT_SCHEMA = pa.schema(
    [
        pa.field("row", pa.uint32()),
        pa.field("lat", pa.float64()),
        pa.field("lon", pa.float64()),
        pa.field("coordinateuncertaintyinmeters", pa.float64()),
        pa.field("gbif_license", pa.string()),
    ]
)


def shards_dir() -> Path:
    path = sources.staging() / "gbif-parts"
    path.mkdir(exist_ok=True)
    return path


def chunked_to_numpy(col) -> np.ndarray:
    """A string `ChunkedArray` to a numpy object array, chunk by chunk.

    Not `combine_chunks().to_numpy(...)`: that concatenates into one Arrow buffer first, whose
    offsets are `int32` for a plain `string` column, which the full 222,660,750-row `source_id`
    column overflows well before any per-call memory limit is the binding constraint. Concatenating
    in numpy instead never builds that buffer — moot now that `build_ids` never holds the whole
    column anyway, but `scan()`'s per-part `gbifid` column still uses it for consistency.
    """
    return np.concatenate([chunk.to_numpy(zero_copy_only=False) for chunk in col.chunks])


def hash_strings(values: np.ndarray) -> np.ndarray:
    """`pandas.util.hash_array` on an object array of strings — deterministic across processes."""
    return pd.util.hash_array(values)


def peak_rss_gb() -> float:
    return resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 2**20


# --------------------------------------------------------------------------------- the id set


def build_ids(limit: int | None = None) -> dict:
    """`source_id` of every `gbif`-sourced TreeOfLife row, as `(hash, row)`, sorted by hash.

    Streams `metadata.parquet` one row group at a time (a row group is one source file, ≤350,000
    rows) rather than materialising the whole `source_id` column — see the module docstring for
    why the first version of this function did not survive. `--limit N` reads only the first N row
    groups, for the RSS smoke this function's own docstring calls for.
    """
    t0 = time.time()
    metadata_path = sources.staging() / "metadata.parquet"
    pf = pq.ParquetFile(metadata_path)
    n_groups = pf.metadata.num_row_groups
    if limit is not None:
        n_groups = min(limit, n_groups)

    # Worst case: every row in every processed group is `gbif`. Trimmed to the real count below —
    # a slice, not a copy, so the trim itself costs nothing. One structured array, not two flat
    # ones plus a separately-materialised sort order: `ndarray.sort(order=...)` on this sorts
    # `hash` and `row` together, in place, over one 12-B/row buffer, rather than the ~4x-the-data
    # transient peak an `argsort` + two independent gathers cost (measured: 11.47 GB against a 2.75
    # GB streaming peak on the full 222,660,750-row run — the sort step, not the streaming, is what
    # blew the budget the first time this was fixed; `README-join.md` §2 carries both figures).
    cap = n_groups * sources.ROWS_PER_FILE
    pairs = np.empty(cap, dtype=[("hash", "<u8"), ("row", "<u4")])
    cursor = 0

    for i in range(n_groups):
        group = pf.read_row_group(i, columns=["row", "source_dataset", "source_id"])
        mask = pc.equal(group["source_dataset"], "gbif")
        matched = group.filter(mask)
        n = matched.num_rows
        if n:
            ids = chunked_to_numpy(matched["source_id"])  # this row group's gbif rows only
            pairs["hash"][cursor : cursor + n] = hash_strings(ids)
            pairs["row"][cursor : cursor + n] = np.asarray(matched["row"])
            cursor += n
        del group, matched
        if (i + 1) % 100 == 0 or i == n_groups - 1:
            print(
                f"{i + 1}/{n_groups} row groups, {cursor:,} gbif rows so far, "
                f"{time.time() - t0:.0f}s (peak {peak_rss_gb():.2f} GB)",
                flush=True,
            )

    pairs = pairs[:cursor]
    pairs.sort(order="hash")  # in place; unstable is fine — ties are a range, not a slot

    hash_col = np.ascontiguousarray(pairs["hash"])
    row_col = np.ascontiguousarray(pairs["row"])
    del pairs

    # `hash_col` is already sorted: counting where it changes is the same answer as `np.unique`
    # for a fraction of the transient memory (no second sort, no separate `unique` copy).
    distinct_hashes = 1 + int(np.count_nonzero(np.diff(hash_col))) if len(hash_col) else 0

    out = sources.staging() / "gbif-ids.parquet"
    pq.write_table(
        pa.table({"hash": pa.array(hash_col, pa.uint64()), "row": pa.array(row_col, pa.uint32())},
                  schema=IDS_SCHEMA),
        out,
        compression="zstd",
    )
    del hash_col, row_col

    summary = {
        "row_groups": n_groups,
        "gbif_rows": int(cursor),
        "distinct_hashes": distinct_hashes,
        "bytes_on_disk": out.stat().st_size,
        "peak_rss_gb": round(peak_rss_gb(), 2),
        "seconds": round(time.time() - t0, 1),
    }
    (sources.staging() / "gbif-ids.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))
    return summary


# ------------------------------------------------------------------------------------ the scan


def expand_ranges(lo: np.ndarray, hi: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """For each query `i` with a nonempty `[lo[i], hi[i])`, every position in that range.

    Returns `(query_index, target_position)`, repeated once per candidate — the vectorised form of
    "for i, (a, b) in enumerate(zip(lo, hi)): for p in range(a, b): yield i, p", needed because one
    `gbifid` can match several TreeOfLife rows (duplicate `source_id`s, see module docstring).
    """
    counts = hi - lo
    matched = np.nonzero(counts > 0)[0]
    if len(matched) == 0:
        return np.empty(0, np.int64), np.empty(0, np.int64)
    c = counts[matched]
    total = int(c.sum())
    query_index = np.repeat(matched, c)
    group_start = np.repeat(np.cumsum(c) - c, c)
    within_group = np.arange(total) - group_start
    target_position = np.repeat(lo[matched], c) + within_group
    return query_index, target_position


def scan(limit: int | None = None) -> dict:
    """One pass over the 8,369 GBIF parts, matched by hash — see the module docstring's stated
    collision risk. Resumable per part."""
    wanted = pq.read_table(sources.staging() / "gbif-ids.parquet")
    sorted_hash = np.asarray(wanted["hash"])
    sorted_row = np.asarray(wanted["row"])
    del wanted

    shards = shards_dir()
    ledger_path = shards / "ledger.jsonl"
    done: set[str] = set()
    if ledger_path.exists():
        for line in ledger_path.read_text().splitlines():
            if line.strip():
                done.add(json.loads(line)["part"])

    todo = sources.gbif_parts()
    if limit is not None:
        todo = todo[:limit]

    t0 = time.time()
    scanned = matched_total = read_bytes = rows_seen = 0
    with open(ledger_path, "a") as ledger:
        for i, path in enumerate(todo):
            if path.name in done:
                continue
            part_t0 = time.time()
            table = pq.ParquetFile(path).read(columns=sources.GBIF_COLUMNS, use_threads=True)
            nbytes = path.stat().st_size

            gbifid = chunked_to_numpy(table["gbifid"])
            key = hash_strings(gbifid)
            lo = np.searchsorted(sorted_hash, key, side="left")
            hi = np.searchsorted(sorted_hash, key, side="right")
            qi, wi = expand_ranges(lo, hi)
            n = len(qi)

            if n:
                lat = np.asarray(table["decimallatitude"])[qi]
                lon = np.asarray(table["decimallongitude"])[qi]
                unc = np.asarray(table["coordinateuncertaintyinmeters"])[qi]
                license_ = chunked_to_numpy(table["license"])[qi]
                shard = pa.table(
                    {
                        "row": pa.array(sorted_row[wi], pa.uint32()),
                        "lat": pa.array(lat, pa.float64()),
                        "lon": pa.array(lon, pa.float64()),
                        "coordinateuncertaintyinmeters": pa.array(unc, pa.float64()),
                        "gbif_license": pa.array(license_, pa.string()),
                        "gbifid": pa.array(gbifid[qi], pa.string()),
                    },
                    schema=SHARD_SCHEMA,
                )
                pq.write_table(
                    shard, shards / (path.name + ".shard"), compression="zstd",
                    use_dictionary=["gbif_license"],
                )

            record = {
                "part": path.name,
                "rows": table.num_rows,
                "matched": n,
                "bytes": nbytes,
                "seconds": round(time.time() - part_t0, 3),
            }
            ledger.write(json.dumps(record) + "\n")
            ledger.flush()
            scanned += 1
            matched_total += n
            read_bytes += nbytes
            rows_seen += table.num_rows
            if scanned % 100 == 0 or i == len(todo) - 1:
                wall = time.time() - t0
                print(
                    f"{scanned}/{len(todo) - len(done)} parts this run, {matched_total:,} matched, "
                    f"{read_bytes / 1e6:.0f} MB in {wall:.0f}s "
                    f"({read_bytes / 1e6 / max(wall, 1e-9):.2f} MB/s, peak {peak_rss_gb():.2f} GB)",
                    flush=True,
                )

    wall = time.time() - t0
    return {
        "parts_scanned": scanned,
        "parts_skipped": len(done),
        "rows_seen": rows_seen,
        "matched": matched_total,
        "bytes": read_bytes,
        "seconds": round(wall, 1),
        "MBps": round(read_bytes / 1e6 / max(wall, 1e-9), 2),
        "peak_rss_gb": round(peak_rss_gb(), 2),
    }


# --------------------------------------------------------------------------------- the combine


def verify_sample(table: pa.Table, n: int, rng: np.random.Generator) -> dict:
    """A hundred matches, checked by string equality against `metadata.parquet` — not the hash.

    `table` carries `row` and the audit-only `gbifid` (dropped from the final output). For each
    sampled row, `row // ROWS_PER_FILE` is the source file / row group (`stage.py` stamps `row =
    arange(offset, offset + n)` in file order, so this is arithmetic, not a search), and the true
    `source_id` is read back from that one row group — a targeted read, not a resident copy of the
    column the rest of this module goes out of its way not to hold.
    """
    total = table.num_rows
    if total == 0:
        return {"sampled": 0, "verified": 0}
    idx = rng.choice(total, size=min(n, total), replace=False)
    sample = table.take(pa.array(idx))
    rows = np.asarray(sample["row"])
    gbifids = chunked_to_numpy(sample["gbifid"])

    pf = pq.ParquetFile(sources.staging() / "metadata.parquet")
    verified = 0
    for row, gbifid in zip(rows, gbifids):
        file_index = int(row) // sources.ROWS_PER_FILE
        position = int(row) % sources.ROWS_PER_FILE
        group = pf.read_row_group(file_index, columns=["row", "source_id"])
        assert group["row"][position].as_py() == int(row), "row group is not in row order"
        source_id = group["source_id"][position].as_py()
        if source_id == gbifid:
            verified += 1
    return {"sampled": len(sample), "verified": verified}


def combine(sample_n: int = 100) -> dict:
    """Shards -> `gbif-coordinates.parquet`, plus the spot check and `join.json`."""
    t0 = time.time()
    shard_paths = sorted(glob.glob(str(shards_dir() / "*.shard")))
    tables = [pq.read_table(p, schema=SHARD_SCHEMA) for p in shard_paths]
    table = pa.concat_tables(tables) if tables else pa.table({}, schema=SHARD_SCHEMA)
    del tables

    rng = np.random.default_rng(0)
    check = verify_sample(table, sample_n, rng)
    assert check["verified"] == check["sampled"], (
        f"string-equality spot check failed: {check['verified']}/{check['sampled']} — "
        "a hash match disagreed with the real strings; the collision risk in the module docstring "
        "was meant to be small, not this small a sample catching one"
    )

    out_table = table.select(["row", "lat", "lon", "coordinateuncertaintyinmeters", "gbif_license"])
    out = sources.staging() / "gbif-coordinates.parquet"
    pq.write_table(out_table, out, compression="zstd", use_dictionary=["gbif_license"])

    matched_rows = out_table.num_rows
    with_coords = int(
        matched_rows - pc.sum(pc.is_null(out_table["lat"])).as_py() if matched_rows else 0
    )

    ids_summary = json.loads((sources.staging() / "gbif-ids.json").read_text())
    metadata_summary = json.loads((sources.staging() / "metadata.json").read_text())
    total_rows = metadata_summary["rows"]
    gbif_rows = ids_summary["gbif_rows"]

    ledger_records = [
        json.loads(line)
        for line in (shards_dir() / "ledger.jsonl").read_text().splitlines()
        if line.strip()
    ]
    scan_bytes = sum(r["bytes"] for r in ledger_records)
    scan_seconds = sum(r["seconds"] for r in ledger_records)

    summary = {
        "rows_total": total_rows,
        "source_dataset_counts": metadata_summary["source_dataset_counts"],
        "gbif_rows_attempted": gbif_rows,
        "gbif_distinct_hashes": ids_summary["distinct_hashes"],
        "matched_rows": matched_rows,
        "matched_with_coordinates": with_coords,
        "join_rate_of_all_rows": round(with_coords / total_rows, 6),
        "join_rate_of_gbif_rows": round(with_coords / gbif_rows, 6) if gbif_rows else 0.0,
        "join_rate_of_matched_rows_with_coordinates": (
            round(with_coords / matched_rows, 6) if matched_rows else 0.0
        ),
        "string_equality_spot_check": check,
        "scan": {
            "parts": len(ledger_records),
            "bytes": scan_bytes,
            "sum_of_part_seconds": round(scan_seconds, 1),
            "MBps_by_part_seconds": round(scan_bytes / 1e6 / max(scan_seconds, 1e-9), 2),
        },
        "bytes_on_disk": out.stat().st_size,
        "seconds": round(time.time() - t0, 1),
    }
    (sources.staging() / "join.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))
    return summary


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--ids", action="store_true", help="the id set, from metadata.parquet")
    ap.add_argument("--scan", action="store_true", help="the one scan of GBIF, resumable")
    ap.add_argument("--combine", action="store_true", help="shards -> gbif-coordinates.parquet")
    ap.add_argument("--limit", type=int, help="--ids: first N row groups; --scan: first N parts")
    args = ap.parse_args()

    every = not (args.ids or args.scan or args.combine)
    if args.ids or every:
        build_ids(args.limit)
    if args.scan or every:
        print(json.dumps(scan(args.limit), indent=2))
    if args.combine or every:
        combine()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
