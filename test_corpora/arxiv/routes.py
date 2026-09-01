"""The two routes to a position, with the same UMAP on the end of each.

The rung publishes both as views (`corpus.toml`), so the two functions here are deliberately
identical past the point where they differ: the same `UMAP_PARAMS`, the same seed, the same
`n_neighbors`. What differs is only what UMAP is given to lay out.

**Both routes use cuML's UMAP on the GPU** (owner ruling, 2026-09-01: this is a demonstrator, and
speed wins over fidelity to the CPU `umap-learn` pipeline this rung used to run). On one
200,000-point graph cuML lays out in 6.4 s where `umap-learn` took 82.7 s, and at 2.4x10^6 that is
the difference between minutes and an hour. What it costs is that neither view is bit-for-bit the
geometry `docs/evidence/` was measured against — `data/geometry.parquet`, which was itself cuML on
a GPU under no seed. **A figure taken against one of these layouts is not comparable with a figure
taken against another**, and that includes `geometry.parquet`.

Which of the two better preserves the embedding is not a question this rung asks: it is a question
about UMAP, and the rung is a demonstrator (owner ruling, 2026-09-01). What decided which is the
anchor is that `knn` is 3x cheaper and keeps a cluster contiguous in row space — see `README.md`.

`random_state` is fixed, and ⊘ that is enough for `pca64` and not for `knn`: CAGRA's index build
is approximate and takes no seed, so the graph UMAP is handed moves between runs.
"""

from __future__ import annotations

import contextlib
import time

import numpy as np

#: PCA's target width on the `pca64` route. It came from `probes/build_geometry.py`, which is what
#: `data/geometry.parquet` was built with.
PCA_DIM = 64

#: One set of UMAP parameters for both routes, so the layout is never what differs between them.
UMAP_PARAMS = dict(n_neighbors=15, min_dist=0.1, n_components=2)

SEED = 0


@contextlib.contextmanager
def _step(name: str, out: dict):
    t = time.time()
    yield
    out[name] = time.time() - t
    print(f"    {name}: {out[name]:.1f}s", flush=True)


def pca64(X: np.ndarray, t: dict) -> np.ndarray:
    """PCA to 64 components, then UMAP over the reduced matrix.

    The covariance is accumulated in blocks because centring the whole matrix at once copies it.
    """
    n = len(X)
    with _step("pca", t):
        mu = X.mean(axis=0)
        cov = np.zeros((X.shape[1], X.shape[1]), dtype=np.float64)
        for lo in range(0, n, 200_000):
            d = (X[lo : lo + 200_000] - mu).astype(np.float64)
            cov += d.T @ d
        cov /= n
        evals, evecs = np.linalg.eigh(cov)
        basis = evecs[:, ::-1][:, :PCA_DIM].astype(np.float32)
        reduced = np.empty((n, PCA_DIM), dtype=np.float32)
        for lo in range(0, n, 200_000):
            reduced[lo : lo + 200_000] = (X[lo : lo + 200_000] - mu) @ basis
        t["pca_variance_kept"] = float(evals[::-1][:PCA_DIM].sum() / evals.sum())
        print(f"    {PCA_DIM} components keep {100 * t['pca_variance_kept']:.1f}% of the variance")

    from cuml.manifold import UMAP

    with _step("umap", t):
        xy = UMAP(**UMAP_PARAMS, random_state=SEED, output_type="numpy").fit_transform(reduced)
    return np.asarray(xy, dtype=np.float32)


def knn(X: np.ndarray, t: dict, *, graph_degree: int = 32, itopk: int = 128,
        half: bool = True) -> np.ndarray:
    """A cosine kNN graph in **full dimension** on the GPU, handed to UMAP as a `precomputed_knn`
    so UMAP does the layout and nothing else.

    This is what removes PCA's reason for existing. `umap-learn` would build the same graph itself
    with `metric="cosine"`; what the GPU index buys is that the graph is affordable at the scale the
    ladder's embedding rungs need, and that the vectors are read once rather than held — the graph
    is `n x k x 8` bytes and the vectors never all need to be resident.

    **The index is built in fp16 by default.** Measured at 200,000 x 1024: recall@15 against an
    exact fp32 brute force is 99.86% in fp16 against 99.88% in fp32, so the precision costs nothing
    that can be seen, and it halves what the card holds — which is what makes 2.4x10^6 x 1024 fit
    in a 10 GB GPU at all.
    """
    import cupy as cp
    from cuvs.neighbors import cagra

    k = UMAP_PARAMS["n_neighbors"]
    with _step("knn", t):
        d = cp.asarray(X.astype(np.float16) if half else X)
        index = cagra.build(
            cagra.IndexParams(metric="cosine", intermediate_graph_degree=2 * graph_degree,
                              graph_degree=graph_degree),
            d,
        )
        nb = cp.empty((len(X), k), dtype=cp.uint32)
        ds = cp.empty((len(X), k), dtype=cp.float32)
        cagra.search(cagra.SearchParams(itopk_size=itopk), index, d, k, neighbors=nb, distances=ds)
        cp.cuda.runtime.deviceSynchronize()
        nbi = cp.asnumpy(nb).astype(np.int32)
        nbd = cp.asnumpy(ds).astype(np.float32)
        del d, index, nb, ds
        cp.get_default_memory_pool().free_all_blocks()

    # UMAP requires each point's own row first in its neighbour list. An approximate index returns
    # it later, or not at all, wherever several rows are identical — arXiv has duplicate abstracts,
    # so this fires on about 0.6% of rows at 200,000. Repaired rather than trusted: a row whose
    # first neighbour is not itself would have UMAP treat some other paper as its own position.
    t["knn_self_first"] = float((nbi[:, 0] == np.arange(len(X))).mean())
    rows = np.arange(len(X), dtype=np.int32)
    wrong = nbi[:, 0] != rows
    if wrong.any():
        nbi[wrong, 0] = rows[wrong]
        nbd[wrong, 0] = 0.0
    np.clip(nbd, 0.0, None, out=nbd)  # cosine distance is non-negative; fp16 rounds a few below 0

    from cuml.manifold import UMAP

    with _step("umap", t):
        # cuML reads the graph and never touches `X` beyond its shape, so the 9.9 GB matrix is
        # not copied to the card here — only the kNN index above needed it there.
        xy = UMAP(**UMAP_PARAMS, metric="cosine", random_state=SEED, output_type="numpy",
                  precomputed_knn=(nbi, nbd)).fit_transform(X)
    return np.asarray(xy, dtype=np.float32)


#: The view names are the route names: a figure quoted against a view says which projection
#: produced it without a lookup. `knn` is first, and is the rung's anchor view.
ROUTES = {"knn": knn, "pca64": pca64}
