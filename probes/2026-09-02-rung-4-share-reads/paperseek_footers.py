"""PaperSeek chunk footers: rows, row groups, per-column sizes, over SMB.

Reads only parquet footers (`ParquetFile(...)`, no `.read()`) for every chunk under
`paperseek-openalex/2026-08-27/`, and separately times one whole-chunk sequential read
and one `id,title,abstract`-projected read of a single chunk. Read-only against the
share; writes only the JSON/CSV results under this probe directory.
"""

from __future__ import annotations

import glob
import json
import time
from pathlib import Path

import pyarrow.parquet as pq

BASE = "/mnt/nas/tessera/datasets/paperseek-openalex/2026-08-27"
OUT = Path(__file__).parent


def footer_survey() -> list[dict]:
    chunks = sorted(
        glob.glob(f"{BASE}/chunk_*.parquet"),
        key=lambda p: int(Path(p).stem.split("_")[1]),
    )
    rows = []
    for path in chunks:
        t0 = time.time()
        pf = pq.ParquetFile(path)
        footer_s = time.time() - t0
        meta = pf.metadata
        per_col = {}
        for rg_idx in range(meta.num_row_groups):
            rg = meta.row_group(rg_idx)
            for c in range(rg.num_columns):
                col = rg.column(c)
                name = col.path_in_schema
                d = per_col.setdefault(name, {"compressed": 0, "uncompressed": 0})
                d["compressed"] += col.total_compressed_size
                d["uncompressed"] += col.total_uncompressed_size
        rows.append(
            {
                "chunk": Path(path).name,
                "footer_read_s": footer_s,
                "num_rows": meta.num_rows,
                "num_row_groups": meta.num_row_groups,
                "file_size_bytes": Path(path).stat().st_size,
                "columns": per_col,
            }
        )
        print(f"{Path(path).name}: {meta.num_rows} rows, {meta.num_row_groups} rgs, footer {footer_s:.3f}s")
    return rows


def timed_reads(chunk_path: str) -> dict:
    size = Path(chunk_path).stat().st_size

    t0 = time.time()
    pq.read_table(chunk_path, use_threads=True)
    whole_s = time.time() - t0
    whole_mbps = (size / 1e6) / whole_s

    t0 = time.time()
    tbl = pq.read_table(chunk_path, columns=["id", "title", "abstract"], use_threads=True)
    proj_s = time.time() - t0
    # bytes actually asked for, from the footer, not the whole-file size
    pf = pq.ParquetFile(chunk_path)
    proj_bytes = 0
    for rg_idx in range(pf.metadata.num_row_groups):
        rg = pf.metadata.row_group(rg_idx)
        for c in range(rg.num_columns):
            col = rg.column(c)
            if col.path_in_schema in ("id", "title", "abstract"):
                proj_bytes += col.total_compressed_size
    proj_mbps = (proj_bytes / 1e6) / proj_s

    return {
        "chunk": Path(chunk_path).name,
        "file_size_bytes": size,
        "whole_read_s": whole_s,
        "whole_MBps": whole_mbps,
        "projected_bytes": proj_bytes,
        "projected_rows": tbl.num_rows,
        "projected_read_s": proj_s,
        "projected_MBps": proj_mbps,
    }


if __name__ == "__main__":
    footers = footer_survey()
    total_rows = sum(r["num_rows"] for r in footers)
    print(f"\nTotal rows across {len(footers)} chunks: {total_rows}")

    timing = timed_reads(f"{BASE}/chunk_0.parquet")
    print(json.dumps(timing, indent=2))

    (OUT / "paperseek_footers.json").write_text(
        json.dumps({"chunks": footers, "total_rows": total_rows}, indent=2)
    )
    (OUT / "paperseek_timing.json").write_text(json.dumps(timing, indent=2))
