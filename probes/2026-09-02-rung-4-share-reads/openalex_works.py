"""OpenAlex `works` over SMB: footer cost, column-pruned reads, whole-part reads.

`works` is Hive-partitioned across 2,428 parts under `openalex/2026-08-27/parquet/works/`.
This times, on 20 parts spread across the partition range: (a) footer-only, (b) a
projected read of `id, publication_year, type, primary_topic, open_access,
best_oa_location` (the licence lives at `best_oa_location.license`, confirmed by
sampling — there is no top-level `license` or `ids.license` column despite the
dataset README naming one), (c) the whole part. It also reads every part's footer
(cheap: local `manifest.json` already carries row counts, cross-checked against a
live footer read on the 20 chosen parts) to give the row total across all 2,428 and
to time that pass.
"""

from __future__ import annotations

import glob
import json
import time
from pathlib import Path

import pyarrow.parquet as pq

BASE = "/mnt/nas/joe/tessera/datasets/openalex/2026-08-27/parquet/works"
MANIFEST = "/mnt/nas/joe/tessera/datasets/openalex/2026-08-27/parquet/manifest.json"
OUT = Path(__file__).parent

PROJECT_COLS = ["id", "publication_year", "type", "primary_topic", "open_access", "best_oa_location"]


def list_parts() -> list[str]:
    parts = sorted(glob.glob(f"{BASE}/*/part_*.parquet"))
    return parts


def pick_20(parts: list[str]) -> list[str]:
    n = len(parts)
    step = n / 20
    idxs = sorted({min(n - 1, int(i * step)) for i in range(20)})
    while len(idxs) < 20:
        # pad if rounding collapsed some
        for i in range(n):
            if i not in idxs:
                idxs.append(i)
                idxs = sorted(idxs)
                break
    return [parts[i] for i in idxs[:20]]


def time_footer(path: str) -> tuple[float, pq.ParquetFile]:
    t0 = time.time()
    pf = pq.ParquetFile(path)
    return time.time() - t0, pf


def projected_bytes(pf: pq.ParquetFile, cols: list[str]) -> int:
    total = 0
    for rg_idx in range(pf.metadata.num_row_groups):
        rg = pf.metadata.row_group(rg_idx)
        for c in range(rg.num_columns):
            col = rg.column(c)
            top = col.path_in_schema.split(".")[0]
            if top in cols:
                total += col.total_compressed_size
    return total


def measure_20(paths: list[str]) -> list[dict]:
    results = []
    for path in paths:
        size = Path(path).stat().st_size
        footer_s, pf = time_footer(path)
        rows = pf.metadata.num_rows

        # pq.read_table() triggers dataset-level schema unification across the
        # Hive `updated_date=` partitions, which fails here (the partition column
        # and an in-file `updated_date` timestamp column collide). Read the single
        # file directly instead.
        t0 = time.time()
        tbl = pf.read(columns=PROJECT_COLS, use_threads=True)
        proj_s = time.time() - t0
        proj_bytes = projected_bytes(pf, PROJECT_COLS)
        proj_mbps = (proj_bytes / 1e6) / proj_s if proj_s > 0 else float("inf")

        t0 = time.time()
        pf.read(use_threads=True)
        whole_s = time.time() - t0
        whole_mbps = (size / 1e6) / whole_s if whole_s > 0 else float("inf")

        rec = {
            "part": str(Path(path).relative_to(BASE)),
            "rows": rows,
            "file_size_bytes": size,
            "footer_read_s": footer_s,
            "projected_bytes": proj_bytes,
            "projected_read_s": proj_s,
            "projected_MBps": proj_mbps,
            "projected_rows_out": tbl.num_rows,
            "whole_read_s": whole_s,
            "whole_MBps": whole_mbps,
        }
        results.append(rec)
        print(
            f"{rec['part']}: rows={rows} size={size/1e6:.1f}MB "
            f"footer={footer_s:.3f}s proj={proj_s:.2f}s({proj_mbps:.1f}MB/s) "
            f"whole={whole_s:.2f}s({whole_mbps:.1f}MB/s)"
        )
    return results


def all_footers(parts: list[str]) -> tuple[list[dict], float]:
    t0 = time.time()
    rows = []
    for path in parts:
        pf = pq.ParquetFile(path)
        rows.append({"part": str(Path(path).relative_to(BASE)), "rows": pf.metadata.num_rows,
                     "file_size_bytes": Path(path).stat().st_size})
    return rows, time.time() - t0


if __name__ == "__main__":
    parts = list_parts()
    print(f"total parts: {len(parts)}")
    chosen = pick_20(parts)
    print(f"chosen {len(chosen)} parts across the range")

    twenty = measure_20(chosen)
    (OUT / "openalex_20parts.json").write_text(json.dumps(twenty, indent=2))

    manifest = json.loads(Path(MANIFEST).read_text())
    works_entity = next(e for e in manifest["entities"] if e["entity"] == "works")
    manifest_total_rows = works_entity["record_count"]
    manifest_total_bytes = works_entity["content_length"]
    manifest_file_count = len(works_entity["files"])
    print(f"manifest.json: {manifest_total_rows} rows, {manifest_total_bytes} bytes, {manifest_file_count} files (claimed)")

    do_all = True
    if do_all:
        footers, all_s = all_footers(parts)
        total_rows = sum(f["rows"] for f in footers)
        print(f"all {len(parts)} footers read in {all_s:.1f}s, total rows = {total_rows}")
        (OUT / "openalex_all_footers.json").write_text(
            json.dumps({"parts": footers, "total_rows": total_rows, "elapsed_s": all_s}, indent=2)
        )

    (OUT / "openalex_manifest_summary.json").write_text(
        json.dumps(
            {
                "manifest_total_rows": manifest_total_rows,
                "manifest_total_bytes": manifest_total_bytes,
                "manifest_file_count": manifest_file_count,
            },
            indent=2,
        )
    )
