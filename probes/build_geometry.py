"""Build geometry.parquet — the one hashed artifact.

Pipeline: embeddings (1024-d BGE, float32) -> exact PCA to 64 components
(CPU, covariance eigendecomposition — VRAM holds neither 2.4M x 1024 nor
the UMAP knn graph over it) -> cuML UMAP to 2D (GPU) -> quantise to the
2^16 grid -> Morton codes -> row_id by Morton rank, entity_id as the
intra-cell tiebreak (priority stands in for it later).

GPU UMAP is not bit-reproducible even under a fixed random_state, which
is exactly why geometry is hashed rather than (seed, config): build once,
record the hash, reuse the artifact. The hash is printed and stored next
to the parquet in geometry.sha256.

Output columns: entity_id, x, y (float32 UMAP coords), gx, gy (u16 grid),
morton (u32), row_id (u32 = Morton rank). Row order: row_id.

Usage: build_geometry.py <corpus.parquet> <embeds.parquet> <out.parquet>
"""

import hashlib
import sys
import time

import duckdb
import numpy as np
import pyarrow.parquet as pq

CORPUS, EMBEDS, OUT = sys.argv[1], sys.argv[2], sys.argv[3]
DIM, PCA_DIM, GRID_BITS = 1024, 64, 16


def log(msg):
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


con = duckdb.connect()
ent_of_id = dict(con.execute(
    f"SELECT id, entity_id FROM read_parquet('{CORPUS}')").fetchall())
n = len(ent_of_id)

log(f"loading embeddings for {n:,} entities")
X = np.empty((n, DIM), dtype=np.float32)
seen = np.zeros(n, dtype=bool)
pf = pq.ParquetFile(EMBEDS)
for batch in pf.iter_batches(batch_size=65536, columns=["paper_id", "embedding"]):
    ids = batch.column("paper_id").to_pylist()
    emb = np.asarray(batch.column("embedding").flatten(), dtype=np.float32).reshape(len(ids), DIM)
    rows = np.fromiter((ent_of_id[i] for i in ids), dtype=np.int64, count=len(ids))
    X[rows] = emb
    seen[rows] = True
assert seen.all(), f"{(~seen).sum()} entities missing embeddings"

log("L2-normalise (BGE is cosine-conventional; norms arrive at ~49.5 +- 3%)")
for lo in range(0, n, 200_000):
    blk = X[lo:lo + 200_000]
    blk /= np.linalg.norm(blk, axis=1, keepdims=True)

log("exact PCA via covariance (CPU)")
mu = X.mean(axis=0)
cov = np.zeros((DIM, DIM), dtype=np.float64)
for lo in range(0, n, 200_000):
    c = X[lo:lo + 200_000].astype(np.float64) - mu
    cov += c.T @ c
evals, evecs = np.linalg.eigh(cov)
P = evecs[:, ::-1][:, :PCA_DIM].astype(np.float32)
Xp = np.empty((n, PCA_DIM), dtype=np.float32)
for lo in range(0, n, 200_000):
    Xp[lo:lo + 200_000] = (X[lo:lo + 200_000] - mu) @ P
var = evals[::-1][:PCA_DIM].sum() / evals.sum()
log(f"PCA done, {PCA_DIM} components keep {100 * var:.1f}% variance")
del X

log("cuML UMAP (GPU)")
from cuml.manifold import UMAP  # noqa: E402  (import after the long CPU phase)
um = UMAP(n_neighbors=15, min_dist=0.1, n_components=2,
          random_state=0, build_algo="nn_descent", verbose=True)
xy = np.asarray(um.fit_transform(Xp), dtype=np.float32)

log("quantise + Morton + rank")

# 32 bits per axis, and the 16-bit cell is its high half — so the cell a point lands in is
# the same whether it is derived here or from the full-precision value, and the residual is
# what is left over rather than a separately-rounded quantity.
#
# `floor(t * 2**32)`, clamped, is the engine's `cell()` (contracts §2.5) widened from 16 bits
# to 32: `q >> 16 == floor(t * 65536)` clamped to 65535, exactly. An earlier revision of this
# script used `round(t * 65535)`, which is a *different* quantiser from the engine's in both
# the scale factor and the rounding mode. That was invisible because the Morton-input build
# path reads the stored code and never re-quantises — but it makes the corpus and the engine
# disagree about which cell a coordinate belongs to, and there is no way to define a residual
# against a cell boundary the two do not share.
Q_BITS = 32
q = np.empty((n, 2), dtype=np.uint32)
for a in range(2):
    lo, hi = xy[:, a].min(), xy[:, a].max()
    t = (xy[:, a].astype(np.float64) - lo) / (float(hi) - float(lo))
    q[:, a] = np.clip(np.floor(t * 2.0**Q_BITS), 0, 2**Q_BITS - 1).astype(np.uint32)

g = q >> np.uint32(16)          # the 16-bit cell: the code's high half
r = q & np.uint32(0xFFFF)       # the sub-cell residual: its low half


def interleave16(v):
    v = v.astype(np.uint64)
    v = (v | (v << 8)) & 0x00FF00FF00FF00FF
    v = (v | (v << 4)) & 0x0F0F0F0F0F0F0F0F
    v = (v | (v << 2)) & 0x3333333333333333
    v = (v | (v << 1)) & 0x5555555555555555
    return v


# x occupies the EVEN bit positions and y the odd ones — contracts §2.5 and its worked example
# (x=6, y=3 -> 30). This script had the two axes the other way round until 2026-08-02, so the
# `morton`, `gx`/`gy`-derived and `row_id` columns of an artifact built before that are a
# transpose. Nothing read them on any path that could notice: the importer takes this file's
# `x`/`y` and quantises them itself, and `build_scaled_corpus.py` reads `x`/`y` too.
#
# **The shipped `data/geometry.parquet` is NOT regenerated to correct this.** It is the one
# hashed artifact (see this module's doc): GPU UMAP is not bit-reproducible, so the file is
# built once and reused by hash, and rebuilding it would change every coordinate rather than
# just these derived columns. Its `morton`/`row_id` therefore remain transposed and remain
# unread; its `x`/`y`, which everything actually consumes, were never affected.
morton = (interleave16(g[:, 0]) | interleave16(g[:, 1]) << 1).astype(np.uint32)
# Same axis convention as `morton`, so concatenating the two words gives the 64-bit interleave
# of the two 32-bit coordinates.
residual = (interleave16(r[:, 0]) | interleave16(r[:, 1]) << 1).astype(np.uint32)
order = np.lexsort((np.arange(n), morton))  # entity_id as intra-cell tiebreak
row_id = np.empty(n, dtype=np.uint32)
row_id[order] = np.arange(n, dtype=np.uint32)

import pyarrow as pa  # noqa: E402

table = pa.table({
    "entity_id": pa.array(order.astype(np.uint32), pa.uint32()),
    "x": pa.array(xy[order, 0], pa.float32()),
    "y": pa.array(xy[order, 1], pa.float32()),
    "gx": pa.array(g[order, 0].astype(np.uint16), pa.uint16()),
    "gy": pa.array(g[order, 1].astype(np.uint16), pa.uint16()),
    "morton": pa.array(morton[order], pa.uint32()),
    "residual": pa.array(residual[order], pa.uint32()),
    "row_id": pa.array(np.arange(n, dtype=np.uint32), pa.uint32()),
})
pq.write_table(table, OUT)

digest = hashlib.sha256(open(OUT, "rb").read()).hexdigest()
with open(OUT + ".sha256", "w") as f:
    f.write(digest + "\n")
log(f"{OUT}: {n:,} rows, sha256 {digest[:16]}…")
