"""Id spelling, id ordering, and a small matched-extract sample, over SMB.

Answers §8.5's "how are ids spelled" and "is the join a full scan" questions on a
small sample: one PaperSeek chunk's ids against one OpenAlex `works` part's
`id, publication_year, type, primary_topic, open_access, best_oa_location`,
written locally as zstd parquet to see what a matched extract's bytes/row looks
like. Not a full join — 1,867 of 358,712 rows in this one part matched this one
2M-id chunk, which is a small and noisy sample; see the README for how this
compares to the source-bytes-per-row model used for the full 102M-row estimate.
"""

from __future__ import annotations

import os

import pyarrow as pa
import pyarrow.parquet as pq

WORKS_PART = (
    "/mnt/nas/tessera/datasets/openalex/2026-08-27/parquet/works/"
    "updated_date=2026-06-26/part_0038.parquet"
)
PAPERSEEK_CHUNK = "/mnt/nas/tessera/datasets/paperseek-openalex/2026-08-27/chunk_0.parquet"
PROJECT_COLS = ["id", "publication_year", "type", "primary_topic", "open_access", "best_oa_location"]

if __name__ == "__main__":
    pf = pq.ParquetFile(WORKS_PART)
    works = pf.read(columns=PROJECT_COLS)
    print(f"works part rows: {works.num_rows}")

    ps_pf = pq.ParquetFile(PAPERSEEK_CHUNK)
    ps_ids = set(ps_pf.read(columns=["id"]).column("id").to_pylist())
    print(f"paperseek ids sampled from chunk_0: {len(ps_ids)}")
    print("sample paperseek id:", next(iter(ps_ids)))
    print("sample works id:    ", works.column("id")[0].as_py())

    mask = [wid in ps_ids for wid in works.column("id").to_pylist()]
    matched = sum(mask)
    print(f"matched rows: {matched} / {works.num_rows}")

    sub = works.filter(pa.array(mask))
    out = "extract_sample.parquet"
    pq.write_table(sub, out, compression="zstd")
    size = os.path.getsize(out)
    print(f"local zstd extract: {size} bytes, {sub.num_rows} rows, {size / max(1, sub.num_rows):.1f} B/row")
