"""One pass off the share, resumable per chunk — the only time rung 4 reads the publisher's bytes.

The staged acquisition is 235.6 GB over SMB and a whole-file read runs at ~41 MB/s while the float
decode runs beside it (`../../probes/2026-09-02-rung-4-share-reads/`), so a pass is roughly 1.6
hours and **there must be exactly one**. Everything after this script — the layout, the clustering,
`prepare.py` — reads the local ladder directory this writes and never the share again.

**The text and the vectors are one pass, not two.** Both live in the same file; reading `id`,
`title` and `abstract` separately would be a second 45 GB of SMB round trips for columns that are
already on the wire. A row group at a time, all four columns, straight through.

Per chunk `N`, under `$TESSERA_LADDER/paperseek/staging/`:

- **`chunk_NN.parquet`** — `row` (the global row index, which is `entity_id`), `id` (the `W…` part
  of the OpenAlex URL), `title` and `abstract`, one row group per source row group, written as they
  are read. `large_string` throughout: a 2M-row chunk's abstracts are ~2.5 GB of characters, past
  what a 32-bit offset addresses, and rung 3 met that limit at full scale rather than at staging
  time (`../medcpt/README.md`).
- **`vectors.f16`** — one flat `(102_117_343, 1024)` float16 memmap, each chunk written at its
  global offset, with `vectors.json` carrying the shape, the per-chunk offsets and a hash of each
  chunk's footer metadata. A chunk absent from the sidecar is refused by `sources.vectors` rather
  than read as the zeros a sparse file would hand back.

**The vectors are L2-normalised on the way past**, in float32, before the cast — rung 3's
convention, for its reasons: cosine is the metric every later step uses, and normalising makes the
stored magnitudes uniform where float16 has its precision. A zero-norm row is left as zeros and
counted.

**209 GB of `vectors.f16` is a temporary.** `prepare.py --drop-vectors` deletes it once the layout
is written, because nothing after the layout reads it. The per-chunk parquet stays: `prepare.py`
reads it once per run.

**Nothing is held whole.** A source row group is ~285,714 rows, which is 1.17 GB of float32
embedding and ~350 MB of prose; a chunk is seven of them and is never materialised.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import resource
import sys
import time
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

from ..common.paths import ladder
from . import sources

#: `row`, `id`, `title`, `abstract` — the staged parquet's schema, fixed here so every chunk's file
#: is the same shape whatever a row group happened to contain.
SCHEMA = pa.schema(
    [
        pa.field("row", pa.uint32()),
        pa.field("id", pa.large_string()),
        pa.field("title", pa.large_string()),
        pa.field("abstract", pa.large_string()),
    ]
)


def footer_digest(path: Path) -> str:
    """A hash of the chunk's footer facts — rows, row groups, byte size, schema.

    Rung 3 hashed the `.npy` header bytes. A parquet footer has no equivalent single blob worth
    hashing cheaply, so this hashes what a re-fetched or truncated file would change. It is a
    staleness check, not an integrity proof, and is only ever compared against itself.
    """
    f = pq.ParquetFile(path)
    facts = json.dumps(
        {
            "rows": f.metadata.num_rows,
            "row_groups": f.metadata.num_row_groups,
            "bytes": path.stat().st_size,
            "schema": str(f.schema_arrow),
        },
        sort_keys=True,
    )
    return hashlib.sha256(facts.encode()).hexdigest()


def stage_chunk(n: int, share: Path, staging: Path, matrix: np.memmap, offset: int, rows: int) -> dict:
    """One chunk: its parquet, its slice of the matrix, and what it cost."""
    t0 = time.time()
    source = pq.ParquetFile(sources.chunk_path(share, n))
    assert source.metadata.num_rows == rows, f"chunk {n}: footer moved under the offsets"

    prefix = sources.ID_PREFIX
    with_title = with_abstract = zero_norm = bad_prefix = 0
    at = 0
    writer = pq.ParquetWriter(
        staging / f"chunk_{n:02d}.parquet", SCHEMA, compression="zstd", use_dictionary=False
    )
    try:
        for g in range(source.metadata.num_row_groups):
            batch = source.read_row_group(g, columns=["id", "title", "abstract", "embedding"])
            k = batch.num_rows

            ids = batch.column("id").combine_chunks()
            # The join is a plain string equality on the full URL and both sides spell it the same
            # way (the share-reads probe §3). A row that does not is counted and kept whole, since
            # a truncated id would be an id for something else.
            held = pc.starts_with(ids, prefix)
            missed = int(k - pc.sum(held).as_py())
            if missed:
                bad_prefix += missed
                ids = pc.if_else(held, pc.utf8_slice_codeunits(ids, len(prefix)), ids)
            else:
                ids = pc.utf8_slice_codeunits(ids, len(prefix))

            title = batch.column("title").combine_chunks()
            abstract = batch.column("abstract").combine_chunks()
            with_title += k - title.null_count - int(pc.sum(pc.equal(title, "")).as_py() or 0)
            with_abstract += (
                k - abstract.null_count - int(pc.sum(pc.equal(abstract, "")).as_py() or 0)
            )

            writer.write_table(
                pa.table(
                    {
                        "row": pa.array(
                            np.arange(offset + at, offset + at + k, dtype=np.uint32), pa.uint32()
                        ),
                        "id": ids.cast(pa.large_string()),
                        "title": title.cast(pa.large_string()),
                        "abstract": abstract.cast(pa.large_string()),
                    },
                    schema=SCHEMA,
                )
            )
            del ids, title, abstract

            # ------------------------------------------------------ the vectors, this row group
            block = (
                batch.column("embedding")
                .combine_chunks()
                .flatten()
                .to_numpy(zero_copy_only=False)
                .reshape(k, sources.EMBED_DIM)
                # A copy, not a view: Arrow's buffer is immutable and the normalisation is in
                # place. `astype` is where the copy happens, so nothing holds three forms at once.
                .astype(np.float32)
            )
            del batch
            norm = np.linalg.norm(block, axis=1, keepdims=True)
            zero_norm += int((norm == 0).sum())
            np.divide(block, norm, out=block, where=norm > 0)
            matrix[offset + at : offset + at + k] = block.astype(np.float16)
            del block, norm
            at += k
    finally:
        writer.close()
    assert at == rows, f"chunk {n}: wrote {at} rows against {rows}"
    matrix.flush()

    stats = {
        "chunk": n,
        "rows": rows,
        "offset": offset,
        "with_title": with_title,
        "with_abstract": with_abstract,
        "id_without_prefix": bad_prefix,
        "zero_norm_vectors": zero_norm,
        "seconds": round(time.time() - t0, 1),
    }
    print(
        f"chunk {n:2d}  {rows:>9,} rows  title {with_title / rows:5.1%}  "
        f"abstract {with_abstract / rows:5.1%}  zero-norm {zero_norm:>4,}  "
        f"{stats['seconds']:.0f}s  (peak "
        f"{resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 2**20:.1f} GB)",
        flush=True,
    )
    return stats


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--chunks", type=str, default=None,
                    help="comma-separated chunk numbers; default all 53, in numeric order")
    ap.add_argument("--out", type=Path, default=None, help="default $TESSERA_LADDER/paperseek")
    ap.add_argument("--force", action="store_true", help="restage chunks already recorded")
    args = ap.parse_args()

    out = args.out or ladder(sources.RUNG)
    share = sources.share()
    staging = sources.staging()
    present = sorted(
        int(p.stem.split("_")[1]) for p in share.glob("chunk_*.parquet")
    )
    assert present == sorted(sources.CHUNKS), (
        f"the share holds chunks {present} against the entity space's {sorted(sources.CHUNKS)}"
    )
    wanted = (
        [int(c) for c in args.chunks.split(",")] if args.chunks else list(sources.CHUNKS)
    )

    rows, offsets = sources.chunk_offsets(share)
    total = sum(rows)
    by_chunk = dict(zip(sources.CHUNKS, rows))
    at_chunk = dict(zip(sources.CHUNKS, offsets))
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
        digest = footer_digest(sources.chunk_path(share, n))
        held = meta["chunks"].get(str(n))
        if held and held.get("footer_sha256") == digest and not args.force:
            print(f"chunk {n:2d}  already staged", flush=True)
            continue
        stats = stage_chunk(n, share, staging, matrix, at_chunk[n], by_chunk[n])
        stats["footer_sha256"] = digest
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
        "with_title": sum(s["with_title"] for s in done),
        "with_abstract": sum(s["with_abstract"] for s in done),
        "id_without_prefix": sum(s["id_without_prefix"] for s in done),
        "zero_norm_vectors": sum(s["zero_norm_vectors"] for s in done),
        "seconds_this_run": round(time.time() - started, 1),
        "peak_rss_gb": round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 2**20, 2),
        "per_chunk": done,
    }
    (staging / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")

    if staged_rows:
        print(
            f"\n{len(done)}/{len(sources.CHUNKS)} chunks, {staged_rows:,} rows: "
            f"title {summary['with_title'] / staged_rows:.1%}, "
            f"abstract {summary['with_abstract'] / staged_rows:.1%}, "
            f"{summary['id_without_prefix']:,} ids without the URL prefix, "
            f"{summary['zero_norm_vectors']:,} zero-norm vectors "
            f"in {summary['seconds_this_run'] / 60:.1f} minutes"
        )


if __name__ == "__main__":
    sys.exit(main())
