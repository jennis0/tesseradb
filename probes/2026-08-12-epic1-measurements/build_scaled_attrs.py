"""Extend the attributed points file past the real corpus — the coarse-zoom cell's fixture.

`records-and-search.md` §11 item 6 owes the coarse-zoom whole-view row scan **under the sweep's
real parallelism**, and the only attributed points file that exists covers the real arXiv corpus
alone: 2,422,486 items. That is not a measurement of the cell. Both category columns fit in this
machine's 32 MiB L3 at that scale (`archive` is 2.4 MB, `primary_category` 4.8 MB), so a
whole-view scan there never leaves cache and its per-row constant is not the one a 10^9 view
would pay. Extrapolating from it without saying so would be the exact dishonesty
`docs/agents/writing.md` forbids.

## What this does, and why it is not fabrication

`probes/build_attributes.py` stops at replica 0 because "a wider scale would have to invent
attribute values for 99.8% of its items, which measures the generator rather than the corpus".
That argument is about *values*. This file does not invent values: the scaled corpus is the same
2,422,486 papers repeated as affine transforms of their geometry (`probes/dataset.md` §4.3), so
entity `e` is a replica of source paper `e % 2422486`, and this assigns that paper's own real
attributes to it. Every value written here is a real arXiv value, carried to the entity that is a
transform of the paper it came from.

**What that does and does not license.** It licenses a *timing* fixture: the hot column's byte
width, the code distribution, the match rate and the result bitmap's structure are all the real
corpus's, repeated. It does **not** license a storage or a vocabulary-growth claim — the distinct
value count is fixed at the real corpus's however far this is scaled, so nothing about dictionary
size, minting or skew-at-scale may be read off a bundle built from this. Timing only, and the
memo that reads it says so.

The one respect in which the repetition is visibly not a larger corpus: a replica's rows land in
Morton order under its own transform, so the code sequence along row space is a shuffled
interleaving of the source's rather than 400 concatenated copies of it. That is the property the
scan cares about, and it is the reason to derive rows from the real geometry rather than to
concatenate.

Usage:
    reference/.venv/bin/python probes/2026-08-12-epic1-measurements/build_scaled_attrs.py \
        --limit 100000000 [--data PATH] [--out PATH] [--chunk 5000000]
"""

import argparse
import os
import time

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq

REPLICA = 2_422_486

ap = argparse.ArgumentParser()
ap.add_argument("--limit", type=int, required=True, help="entity_id < LIMIT")
ap.add_argument("--data", default="/home/joe/code/tessera/data")
ap.add_argument("--out", default=None)
ap.add_argument("--chunk", type=int, default=5_000_000, help="rows per output row group")
args = ap.parse_args()

DATA = args.data
GEOM = os.path.join(DATA, "scaled", "geometry.parquet")
SRC = os.path.join(DATA, "scaled", "attrs", "points.parquet")
OUT = args.out or os.path.join(DATA, "scaled", "attrs", f"points-{args.limit}.parquet")

# The source attributes, in entity order. `points.parquet` is `geometry.parquet`'s first replica
# plus the five columns; only the columns are wanted here, and the index is the source entity id.
t0 = time.time()
src = pq.read_table(SRC, columns=["entity_id", "archive", "primary_category",
                                  "author_count", "submitted_at", "recency"])
order = np.argsort(src["entity_id"].to_numpy())
assert np.array_equal(src["entity_id"].to_numpy()[order], np.arange(REPLICA)), \
    "points.parquet must cover entity ids 0..2422485 exactly once"
archive = np.asarray(src["archive"].to_pylist(), dtype=object)[order]
primary = np.asarray(src["primary_category"].to_pylist(), dtype=object)[order]
author_count = src["author_count"].to_numpy()[order]
submitted_at = src["submitted_at"].to_numpy()[order]
recency = src["recency"].to_numpy()[order]
print(f"source attributes: {REPLICA:,} rows in {time.time() - t0:.1f}s", flush=True)

schema = pa.schema([
    ("entity_id", pa.uint32()),
    ("morton", pa.uint32()),
    ("residual", pa.uint32()),
    ("archive", pa.string()),
    ("primary_category", pa.string()),
    ("author_count", pa.uint8()),
    ("submitted_at", pa.int64()),
    ("recency", pa.float32()),
])

# `geometry.parquet` is sorted by (morton, entity_id), NOT by entity id, so the rows this scale
# wants are spread across every one of its 1,000 row groups and the whole file must be walked.
# Walked in row-group order and buffered: nothing here needs the output sorted, since `tessera
# build` sorts by morton itself.
geom = pq.ParquetFile(GEOM)
writer = pq.ParquetWriter(OUT, schema, compression="zstd")
kept = 0
buf_e, buf_m, buf_r = [], [], []
buffered = 0
t0 = time.time()


def flush():
    global buffered, buf_e, buf_m, buf_r
    if buffered == 0:
        return
    e = np.concatenate(buf_e)
    m = np.concatenate(buf_m)
    r = np.concatenate(buf_r)
    idx = e % REPLICA
    writer.write_table(pa.table({
        "entity_id": pa.array(e, type=pa.uint32()),
        "morton": pa.array(m, type=pa.uint32()),
        "residual": pa.array(r, type=pa.uint32()),
        "archive": pa.array(archive[idx].tolist(), type=pa.string()),
        "primary_category": pa.array(primary[idx].tolist(), type=pa.string()),
        "author_count": pa.array(author_count[idx], type=pa.uint8()),
        "submitted_at": pa.array(submitted_at[idx], type=pa.int64()),
        "recency": pa.array(recency[idx], type=pa.float32()),
    }, schema=schema))
    buf_e, buf_m, buf_r = [], [], []
    buffered = 0


for rg in range(geom.metadata.num_row_groups):
    batch = geom.read_row_group(rg, columns=["entity_id", "morton", "residual"])
    e = batch["entity_id"].to_numpy()
    keep = e < args.limit
    if not keep.any():
        continue
    buf_e.append(e[keep])
    buf_m.append(batch["morton"].to_numpy()[keep])
    buf_r.append(batch["residual"].to_numpy()[keep])
    buffered += int(keep.sum())
    kept += int(keep.sum())
    if buffered >= args.chunk:
        flush()
    if rg % 100 == 99:
        print(f"  row group {rg + 1}/{geom.metadata.num_row_groups}, kept {kept:,}, "
              f"{time.time() - t0:.0f}s", flush=True)

flush()
writer.close()
print(f"wrote {OUT}: {kept:,} rows in {time.time() - t0:.1f}s, "
      f"{os.path.getsize(OUT) / 1e9:.2f} GB")
if kept != args.limit:
    print(f"NOTE: kept {kept:,} != --limit {args.limit:,} — the corpus has no more entity ids "
          f"below that bound.")
