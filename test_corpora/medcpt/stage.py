"""One pass off the share, resumable per chunk — the only time rung 3 reads the publisher's bytes.

The staged acquisition is 163 GB over SMB at ~67 MB/s (`../README.md`), so a pass is roughly forty
minutes and **there must be exactly one**. Everything after this script — the kNN graph, the
layout, the clusterings, `prepare.py` — reads the local ladder directory this writes and never the
share again.

Per chunk `N`, under `$TESSERA_LADDER/medcpt/staging/`:

- **`chunk_NN.parquet`** — `row` (the global row index, which is `entity_id`), `pmid`, `published`,
  `title`, `abstract` and `mesh` (the raw `m` field, left for the MeSH track to resolve). One row
  per PMID in `pmids_chunk_N.json` order, which *is* the `.npy` row order and is asserted rather
  than assumed: a misalignment here would attach every article's title to another article's vector
  and nothing downstream could see it.
- **`vectors.f16`** — one flat `(35_920_666, 768)` float16 memmap, each chunk written at its global
  offset, with `vectors.json` carrying the shape, the per-chunk offsets and a hash of each chunk's
  `.npy` header. A chunk absent from the sidecar is refused by `sources.vectors` rather than read
  as the zeros a sparse file would hand back.

**The vectors are L2-normalised on the way past**, in float32, before the cast. MedCPT's raw
vectors have norms around 8.7, and cosine is the metric every later step uses; normalising here
makes the stored numbers uniform in magnitude — around 0.036 — which is where float16 has its
precision, and makes cosine an inner product for anything that wants one. A zero-norm row is left
as zeros and counted.

**Nothing is held whole.** One `.npy` chunk is 2.9 GB and one `pubmed_chunk_N.json` is up to 1.8 GB
of JSON that parses to several gigabytes of Python strings, so the vectors are read through
`mmap_mode="r"` and converted in slices, and the content dictionary is consumed by popping as the
columns are built.

**Dates are ignore-and-report.** The `d` field is `YYYYMMDD`; one that does not parse is written
null and counted, and the count goes in `summary.json` and is printed. A rung does not refuse a
build over a malformed date.
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import resource
import sys
import time
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq

from ..common.paths import ladder
from . import sources

#: Rows converted at a time. 65,536 x 768 float32 is 200 MB — large enough that the SMB read is
#: sequential, small enough that two of them are noise against the box.
SLICE = 65_536


def parse_dates(raw: list[str | None]) -> tuple[np.ndarray, int]:
    """`YYYYMMDD` strings to microseconds since the epoch, with a null where it does not parse.

    Returns the column and the count that did not parse. Every failure mode the field actually
    shows — an empty string, a bare year, a month or day of zero — lands in the same place: a null
    and a number in the report.
    """
    out = np.full(len(raw), np.datetime64("NaT"), dtype="datetime64[us]")
    bad = 0
    for i, d in enumerate(raw):
        if d and len(d) == 8 and d.isdigit():
            try:
                out[i] = np.datetime64(
                    datetime.datetime(int(d[:4]), int(d[4:6]), int(d[6:8])), "us"
                )
                continue
            except ValueError:
                pass
        bad += 1
    return out, bad


def stage_chunk(n: int, share: Path, staging: Path, matrix: np.memmap, offset: int, rows: int) -> dict:
    """One chunk: its parquet, its slice of the matrix, and what it cost."""
    t0 = time.time()

    pmids = json.loads((share / f"pmids_chunk_{n}.json").read_bytes())
    assert len(pmids) == rows, f"chunk {n}: {len(pmids)} pmids against {rows} vectors"

    content = json.loads((share / f"pubmed_chunk_{n}.json").read_bytes())
    assert len(content) == rows, f"chunk {n}: {len(content)} contents against {rows} vectors"

    titles: list[str | None] = []
    abstracts: list[str | None] = []
    meshes: list[str | None] = []
    dates: list[str | None] = []
    for pmid in pmids:
        record = content.pop(pmid)
        dates.append(record.get("d"))
        titles.append(record.get("t") or None)
        abstracts.append(record.get("a") or None)
        meshes.append(record.get("m") or None)
    del content
    read_seconds = time.time() - t0

    published, bad_dates = parse_dates(dates)
    del dates

    pq.write_table(
        pa.table(
            {
                "row": pa.array(
                    np.arange(offset, offset + rows, dtype=np.uint32), pa.uint32()
                ),
                "pmid": pa.array(np.array(pmids, dtype=np.uint32), pa.uint32()),
                "published": pa.array(published, pa.timestamp("us")),
                "title": pa.array(titles, pa.string()),
                "abstract": pa.array(abstracts, pa.string()),
                "mesh": pa.array(meshes, pa.string()),
            }
        ),
        staging / f"chunk_{n:02d}.parquet",
        compression="zstd",
    )
    with_mesh = sum(m is not None for m in meshes)
    with_abstract = sum(a is not None for a in abstracts)
    del titles, abstracts, meshes, published, pmids

    # ------------------------------------------------------------------ the vectors, in slices
    t1 = time.time()
    source = np.load(share / f"embeds_chunk_{n}.npy", mmap_mode="r")
    zero_norm = 0
    for lo in range(0, rows, SLICE):
        block = np.array(source[lo : lo + SLICE], dtype=np.float32)
        norm = np.linalg.norm(block, axis=1, keepdims=True)
        zero_norm += int((norm == 0).sum())
        np.divide(block, norm, out=block, where=norm > 0)
        matrix[offset + lo : offset + lo + len(block)] = block.astype(np.float16)
    del source
    matrix.flush()

    stats = {
        "chunk": n,
        "rows": rows,
        "offset": offset,
        "with_mesh": with_mesh,
        "with_abstract": with_abstract,
        "unparseable_dates": bad_dates,
        "zero_norm_vectors": zero_norm,
        "read_seconds": round(read_seconds, 1),
        "vector_seconds": round(time.time() - t1, 1),
        "seconds": round(time.time() - t0, 1),
    }
    print(
        f"chunk {n:2d}  {rows:>9,} rows  mesh {with_mesh / rows:5.1%}  "
        f"abstract {with_abstract / rows:5.1%}  bad dates {bad_dates:>6,}  "
        f"{stats['read_seconds']:.0f}s json + {stats['vector_seconds']:.0f}s vectors  "
        f"(peak {resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 2**20:.1f} GB)",
        flush=True,
    )
    return stats


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--chunks", type=str, default=None,
                    help="comma-separated chunk numbers; default all 38, in order")
    ap.add_argument("--out", type=Path, default=None, help="default $TESSERA_LADDER/medcpt")
    ap.add_argument("--force", action="store_true", help="restage chunks already recorded")
    args = ap.parse_args()

    out = args.out or ladder(sources.RUNG)
    share = sources.share()
    staging = sources.staging(out)
    wanted = (
        [int(c) for c in args.chunks.split(",")] if args.chunks else list(sources.CHUNKS)
    )

    rows, offsets = sources.chunk_offsets(share)
    total = sum(rows)
    print(f"share   {share}\nstaging {staging}\n{total:,} rows x {sources.EMBED_DIM}", flush=True)

    sidecar = staging / "vectors.json"
    meta = (
        json.loads(sidecar.read_text())
        if sidecar.exists()
        else {"rows": total, "dim": sources.EMBED_DIM, "dtype": "float16",
              "normalised": True, "chunks": {}}
    )
    assert meta["rows"] == total, "the sidecar was written against a different chunk set"

    path = staging / "vectors.f16"
    matrix = np.memmap(
        path, dtype=np.float16, mode="r+" if path.exists() else "w+",
        shape=(total, sources.EMBED_DIM),
    )

    started = time.time()
    for n in wanted:
        _, _, header = sources.npy_header(share / f"embeds_chunk_{n}.npy")
        digest = hashlib.sha256(header).hexdigest()
        held = meta["chunks"].get(str(n))
        if held and held["header_sha256"] == digest and not args.force:
            print(f"chunk {n:2d}  already staged", flush=True)
            continue
        stats = stage_chunk(n, share, staging, matrix, offsets[n], rows[n])
        stats["header_sha256"] = digest
        meta["chunks"][str(n)] = stats
        sidecar.write_text(json.dumps(meta, indent=2) + "\n")

    done = [meta["chunks"][str(n)] for n in sources.CHUNKS if str(n) in meta["chunks"]]
    staged_rows = sum(s["rows"] for s in done)
    summary = {
        "rung": sources.RUNG,
        "share": str(share),
        "chunks_staged": len(done),
        "rows": staged_rows,
        "rows_expected": total,
        "with_mesh": sum(s["with_mesh"] for s in done),
        "with_abstract": sum(s["with_abstract"] for s in done),
        "unparseable_dates": sum(s["unparseable_dates"] for s in done),
        "zero_norm_vectors": sum(s["zero_norm_vectors"] for s in done),
        "seconds_this_run": round(time.time() - started, 1),
        "peak_rss_gb": round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 2**20, 2),
        "per_chunk": done,
    }
    (staging / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")

    if staged_rows:
        print(
            f"\n{len(done)}/38 chunks, {staged_rows:,} rows: "
            f"MeSH {summary['with_mesh'] / staged_rows:.1%}, "
            f"abstract {summary['with_abstract'] / staged_rows:.1%}, "
            f"{summary['unparseable_dates']:,} unparseable dates, "
            f"{summary['zero_norm_vectors']:,} zero-norm vectors "
            f"in {summary['seconds_this_run'] / 60:.1f} minutes"
        )


if __name__ == "__main__":
    sys.exit(main())
