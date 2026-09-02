"""The one route to a position at 3.6x10^7 — fit a layout on what the card holds, place the rest.

The arXiv rung's `knn` route puts the whole fp16 matrix on the card, builds one CAGRA index over
it, and hands cuML's UMAP the graph as a `precomputed_knn`. Here the matrix is
35,920,666 x 768 float16 — **55 GB** — which fits neither a 10 GB card nor a 47 GB box, and the
graph over all of it would be ~1.1x10^9 symmetrised edges, which does not fit either. So the route
is the third of the brief's three, taken directly:

1. **Fit.** A uniform sample of `FIT_ROWS` rows goes on the card as one CAGRA index; its own kNN
   graph goes to cuML's UMAP as a `precomputed_knn`; that is the layout.
2. **Place.** Every remaining row is searched against the same index — it is already resident — and
   positioned at the **similarity-weighted mean of its 15 fit-set neighbours' positions**. The fit
   rows are then overwritten with their own UMAP positions, so the route places every row through
   one code path and the reads off the staged memmap stay contiguous.

**Why not lay out all 36M.** Measured on this box (RTX 3080, 10 GB, ~8.2 GB free) before anything
was committed: cuML's UMAP over a precomputed graph peaks at **1,417 bytes a row at 2,000,000** and
**1,283 at 2,500,000**, so 35,920,666 rows is **~46 GB of device memory** — six times the card, and
past the 47 GB of host RAM that RMM's managed memory would have to oversubscribe into. It does not
fit, and no managed-memory run was attempted for that reason.

The owner's framing (2026-09-02) is that this corpus tests Tessera's speed and memory rather than
the UMAP pipeline, so the first route that works is the one taken; nothing about layout fidelity is
measured or claimed.

**Sharded CAGRA is retained and is not used at 36M.** `knn_graph` takes a `shard_rows` and merges
per-shard top-k on the device, which is what a graph over every row would need; the fit-and-place
route makes a graph over every row unnecessary, so at full scale it runs with one shard over the
fit set. Nothing measured the sharded path at 36M — do not quote it.

`SEED` and `UMAP_PARAMS` carry over from the arXiv rung, and so does its ⊘: **CAGRA's index build
takes no seed**, so the graph UMAP is handed differs run to run and the layout with it.

    ~/venvs/projection/bin/python -m test_corpora.medcpt.routes --rows 2000000 --fit 2000000

reports index build time, search throughput, the layout's wall time and the peak device memory each
stage reached.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import sys
import threading
import time
from pathlib import Path

import numpy as np

#: One set of UMAP parameters, and `n_neighbors` is also the graph's `k`.
UMAP_PARAMS = dict(n_neighbors=15, min_dist=0.1, n_components=2)

SEED = 0

#: CAGRA's graph degree and its intermediate, and the search's candidate list. The arXiv rung's
#: values; nothing here re-tuned them, the rung being a demonstrator whose recall is not measured.
GRAPH_DEGREE = 32
ITOPK = 128

#: Rows UMAP is fitted over. The binding constraint is the CAGRA index and not the layout: the
#: index peaked at **5.46 GB at 2,500,000 rows** where the layout over the same graph peaked at
#: 2.99 GB, against ~8.2 GB free on a 10 GB card. 2,500,000 is the largest size actually measured
#: rather than the largest extrapolated, because an OOM two thirds of the way through a 36M run
#: costs an hour and a larger fit set buys nothing this rung measures.
FIT_ROWS = 2_500_000

#: Rows per CAGRA index when a graph is built over more rows than one index holds. Unused at full
#: scale: the fit set is one shard.
SHARD_ROWS = 2_500_000

#: Query rows per search call. 250,000 x 768 float16 is 366 MB on the card, and the read from the
#: staged memmap is sequential at that size.
BATCH_ROWS = 250_000


@contextlib.contextmanager
def _step(name: str, out: dict):
    t = time.time()
    yield
    out[name] = round(time.time() - t, 2)
    print(f"    {name}: {out[name]:.1f}s", flush=True)


class DeviceWatermark:
    """Peak device memory, sampled from the driver rather than from an allocator.

    CuPy's pool reports what CuPy allocated; cuML, cuVS and RMM each have their own. What the
    driver reports free is the only number that covers all of them, and the peak of the card is
    what decides whether a size fits.
    """

    def __init__(self, interval: float = 0.2):
        import cupy as cp

        self.total = cp.cuda.runtime.memGetInfo()[1]
        self.interval = interval
        self.peak = 0
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def _run(self) -> None:
        import cupy as cp

        while not self._stop.wait(self.interval):
            free, total = cp.cuda.runtime.memGetInfo()
            self.peak = max(self.peak, total - free)

    def __enter__(self):
        self._thread.start()
        return self

    def __exit__(self, *exc):
        self._stop.set()
        self._thread.join()

    @property
    def peak_gb(self) -> float:
        return self.peak / 2**30


class Vectors:
    """The staged matrix, read in contiguous slices whatever backs it.

    The full run reads `staging/vectors.f16` through a memmap; a smaller run gathers its uniform
    sample into host RAM first, because a fancy index over a memmap is one read per row. Both
    present the same interface, so nothing below knows which it has.
    """

    def __init__(self, block: np.ndarray, resident: bool):
        self.block = block
        self.resident = resident
        self.n, self.dim = block.shape

    @classmethod
    def memmap(cls, matrix: np.memmap) -> Vectors:
        return cls(matrix, resident=False)

    @classmethod
    def gathered(cls, matrix: np.memmap, take: np.ndarray, chunk: int = 500_000) -> Vectors:
        """A sample, copied into RAM in sorted order — `take` must be sorted."""
        out = np.empty((len(take), matrix.shape[1]), dtype=np.float16)
        for lo in range(0, len(take), chunk):
            out[lo : lo + chunk] = matrix[take[lo : lo + chunk]]
        return cls(out, resident=True)

    def __getitem__(self, sl: slice) -> np.ndarray:
        return np.asarray(self.block[sl])

    def gather(self, rows: np.ndarray, chunk: int = 500_000) -> np.ndarray:
        """Named rows, in sorted order, copied out. Used once, for the fit set."""
        out = np.empty((len(rows), self.dim), dtype=np.float16)
        for lo in range(0, len(rows), chunk):
            out[lo : lo + chunk] = self.block[rows[lo : lo + chunk]]
        return out


def fit_rows_for(n: int, fit: int, seed: int = SEED) -> np.ndarray:
    """The rows UMAP is fitted over — a uniform sample, sorted.

    **Uniform rather than a prefix.** The chunks are in PMID order, so a prefix takes the oldest
    articles, and MeSH coverage, abstract coverage and subject mix all run strongly with time
    (`../../docs/ingest-campaign.md` §4.4). Sorting keeps the gather off the memmap in order.
    """
    if fit >= n:
        return np.arange(n)
    return np.sort(np.random.default_rng(seed).choice(n, fit, replace=False))


def knn_graph(
    block: np.ndarray,
    t: dict,
    *,
    k: int = UMAP_PARAMS["n_neighbors"],
    shard_rows: int = SHARD_ROWS,
    batch_rows: int = BATCH_ROWS,
) -> tuple[np.ndarray, np.ndarray]:
    """A cosine kNN graph over a resident block, one CAGRA index per shard, merged on the device.

    The vectors are staged L2-normalised (`stage.py`), so cosine and inner product agree; the metric
    is still named `cosine` so the index does its own normalisation and a change to the staging
    convention cannot silently change the graph.

    At full scale this runs with **one shard**: the block is the fit set. The merge is kept because
    it is what a graph over every row would need and it costs nothing at one shard.
    """
    import cupy as cp
    from cuvs.neighbors import cagra

    n = len(block)
    shards = [(lo, min(lo + shard_rows, n)) for lo in range(0, n, shard_rows)]
    best_i = np.zeros((n, k), dtype=np.int32)
    best_d = np.full((n, k), np.inf, dtype=np.float32)
    t["shards"], t["shard_rows"] = len(shards), shard_rows
    t["build_seconds"], t["search_seconds"] = [], []

    for s, (slo, shi) in enumerate(shards):
        t0 = time.time()
        resident = cp.asarray(block[slo:shi])
        index = cagra.build(
            cagra.IndexParams(metric="cosine", intermediate_graph_degree=2 * GRAPH_DEGREE,
                              graph_degree=GRAPH_DEGREE),
            resident,
        )
        cp.cuda.runtime.deviceSynchronize()
        t["build_seconds"].append(round(time.time() - t0, 1))

        t1 = time.time()
        params = cagra.SearchParams(itopk_size=ITOPK)
        for lo in range(0, n, batch_rows):
            hi = min(lo + batch_rows, n)
            q = cp.asarray(block[lo:hi])
            nb = cp.empty((hi - lo, k), dtype=cp.uint32)
            ds = cp.empty((hi - lo, k), dtype=cp.float32)
            cagra.search(params, index, q, k, neighbors=nb, distances=ds)
            del q
            cand_i, cand_d = nb.astype(cp.int32) + slo, ds
            if s:
                cand_i = cp.concatenate([cp.asarray(best_i[lo:hi]), cand_i], axis=1)
                cand_d = cp.concatenate([cp.asarray(best_d[lo:hi]), cand_d], axis=1)
                order = cp.argsort(cand_d, axis=1)[:, :k]
                cand_i = cp.take_along_axis(cand_i, order, axis=1)
                cand_d = cp.take_along_axis(cand_d, order, axis=1)
            best_i[lo:hi] = cp.asnumpy(cand_i)
            best_d[lo:hi] = cp.asnumpy(cand_d)
            del nb, ds, cand_i, cand_d
        cp.cuda.runtime.deviceSynchronize()
        search = time.time() - t1
        t["search_seconds"].append(round(search, 1))
        print(f"    shard {s + 1}/{len(shards)}: build {t['build_seconds'][-1]:.1f}s, "
              f"{n:,} queries in {search:.1f}s ({n / search:,.0f}/s)", flush=True)
        del resident, index
        cp.get_default_memory_pool().free_all_blocks()

    # An approximate index does not always return a row first in its own neighbour list, and a row
    # whose first neighbour is not itself would have UMAP place it on another article's position.
    # PubMed carries duplicate titles, so this fires. Repaired rather than trusted.
    rows = np.arange(n, dtype=np.int32)
    t["self_first"] = float((best_i[:, 0] == rows).mean())
    wrong = best_i[:, 0] != rows
    if wrong.any():
        best_i[wrong, 0] = rows[wrong]
        best_d[wrong, 0] = 0.0
    np.clip(best_d, 0.0, None, out=best_d)  # cosine distance is non-negative; fp16 rounds below 0
    return best_i, best_d


def layout(nbi: np.ndarray, nbd: np.ndarray, t: dict, *, managed: bool = False) -> np.ndarray:
    """cuML's UMAP over a precomputed graph.

    **The point matrix is a placeholder.** With `precomputed_knn` cuML reads the graph and takes
    only the row count from `X`, so a `(n, 1)` array of zeros stands in for the vectors and they
    are never on the card at layout time.

    `managed` re-initialises RMM with managed memory first, so the card oversubscribes into host
    RAM. It is not the route taken — `README.md` says what was measured.
    """
    if managed:
        import rmm

        rmm.reinitialize(managed_memory=True, pool_allocator=True)

    from cuml.manifold import UMAP

    with _step("umap", t):
        xy = UMAP(**UMAP_PARAMS, metric="cosine", random_state=SEED, output_type="numpy",
                  precomputed_knn=(nbi, nbd)).fit_transform(
            np.zeros((len(nbi), 1), dtype=np.float32)
        )
    return np.asarray(xy, dtype=np.float32)


def knn(
    X: Vectors,
    t: dict,
    *,
    fit: int = FIT_ROWS,
    shard_rows: int = SHARD_ROWS,
    batch_rows: int = BATCH_ROWS,
    managed: bool = False,
) -> np.ndarray:
    """Every row's position: UMAP over the fit set's own graph, the rest placed against it.

    One CAGRA index serves both halves — it is built for the graph and searched again for the
    placement — so the fit set's vectors go to the card once.
    """
    import cupy as cp
    from cuvs.neighbors import cagra

    n = X.n
    rows = fit_rows_for(n, fit)
    t["fit_rows"] = len(rows)
    with _step("gather fit set", t):
        block = X.gather(rows)

    with _step("knn", t):
        nbi, nbd = knn_graph(block, t, shard_rows=shard_rows, batch_rows=batch_rows)
    fit_xy = layout(nbi, nbd, t, managed=managed)
    del nbi, nbd

    if len(rows) == n:
        return fit_xy

    # ------------------------------------------------------------------------------ the placement
    k = UMAP_PARAMS["n_neighbors"]
    xy = np.empty((n, 2), dtype=np.float32)
    with _step("place", t):
        resident = cp.asarray(block)
        index = cagra.build(
            cagra.IndexParams(metric="cosine", intermediate_graph_degree=2 * GRAPH_DEGREE,
                              graph_degree=GRAPH_DEGREE),
            resident,
        )
        del block
        positions = cp.asarray(fit_xy)
        params = cagra.SearchParams(itopk_size=ITOPK)
        # **Every row, fit rows included.** They are overwritten with their own UMAP positions
        # below; searching them costs 7% and buys contiguous reads off a 55 GB memmap, where
        # skipping them would make every batch a gather.
        for lo in range(0, n, batch_rows):
            hi = min(lo + batch_rows, n)
            q = cp.asarray(X[lo:hi])
            nb = cp.empty((hi - lo, k), dtype=cp.uint32)
            ds = cp.empty((hi - lo, k), dtype=cp.float32)
            cagra.search(params, index, q, k, neighbors=nb, distances=ds)
            # Similarity, not distance: a neighbour at distance 0 must dominate the mean, and a
            # cosine distance of 1 is orthogonal and should count for nothing.
            w = cp.clip(1.0 - ds, 0.0, None) + 1e-6
            w /= w.sum(axis=1, keepdims=True)
            got = positions[nb.astype(cp.int32)]
            xy[lo:hi] = cp.asnumpy((got * w[:, :, None]).sum(axis=1))
            del q, nb, ds, w, got
        cp.cuda.runtime.deviceSynchronize()
        del resident, index, positions
        cp.get_default_memory_pool().free_all_blocks()
    xy[rows] = fit_xy
    return xy


#: One view, and its name is the route's. The publisher says MedCPT's geometry is organised for
#: retrieval rather than for topic, so the view is titled *Literature map* and not a topic map.
ROUTES = {"knn": knn}


# ------------------------------------------------------------------ the measurement driver


def main() -> None:
    ap = argparse.ArgumentParser(description="measure the route before committing a 36M run")
    ap.add_argument("--rows", type=int, default=2_000_000, help="uniform sample size; 0 takes all")
    ap.add_argument("--fit", type=int, default=None,
                    help="rows UMAP is fitted over; default all of --rows")
    ap.add_argument("--shard", type=int, default=SHARD_ROWS, help="rows per CAGRA index")
    ap.add_argument("--managed", action="store_true", help="RMM managed memory for the layout")
    ap.add_argument("--graph-only", action="store_true", help="stop after the kNN graph")
    ap.add_argument("--out", type=Path, default=None, help="default $TESSERA_LADDER/medcpt")
    ap.add_argument("--report", type=Path, default=None, help="append the timings here as JSON")
    ap.add_argument("--partial", action="store_true",
                    help="sample the staged prefix while the staging pass is still running")
    args = ap.parse_args()

    from ..common.paths import ladder
    from . import sources

    out = args.out or ladder(sources.RUNG)
    matrix, meta = sources.vectors(complete=not args.partial)
    n_full = sources.staged_rows(meta) if args.partial else meta["rows"]
    if args.partial:
        print(f"⊘ measuring against the staged prefix: {n_full:,} of {meta['rows']:,} rows",
              flush=True)
    n = n_full if not args.rows else min(args.rows, n_full)

    t: dict = {"rows": n, "corpus": n_full, "fit": args.fit or n, "managed": args.managed,
               "measured_at": time.strftime("%Y-%m-%d %H:%M")}
    if n == meta["rows"]:
        X = Vectors.memmap(matrix)
    else:
        take = np.sort(np.random.default_rng(SEED).choice(n_full, n, replace=False))
        with _step("gather", t):
            X = Vectors.gathered(matrix, take)
    print(f"{n:,} rows x {X.dim}, {'resident' if X.resident else 'memmap'}", flush=True)

    rows = fit_rows_for(n, args.fit or n)
    with DeviceWatermark() as watch:
        block = X.gather(rows) if len(rows) < n else X[0:n]
        with _step("knn", t):
            nbi, nbd = knn_graph(block, t, shard_rows=args.shard)
    t["graph_peak_vram_gb"] = round(watch.peak_gb, 2)
    print(f"  graph peak device memory {t['graph_peak_vram_gb']:.2f} GB, "
          f"self-first {t['self_first']:.3%}", flush=True)

    if not args.graph_only:
        with DeviceWatermark() as watch:
            xy = layout(nbi, nbd, t, managed=args.managed)
        t["layout_peak_vram_gb"] = round(watch.peak_gb, 2)
        t["layout_vram_bytes_per_row"] = round(watch.peak / len(nbi), 1)
        t["bounds"] = {"x": [float(xy[:, 0].min()), float(xy[:, 0].max())],
                       "y": [float(xy[:, 1].min()), float(xy[:, 1].max())]}
        print(f"  layout peak device memory {t['layout_peak_vram_gb']:.2f} GB "
              f"({t['layout_vram_bytes_per_row']:.0f} bytes/row), "
              f"x {t['bounds']['x']}, y {t['bounds']['y']}", flush=True)

    print(json.dumps(t, indent=2))
    if args.report:
        held = json.loads(args.report.read_text()) if args.report.exists() else []
        held.append(t)
        args.report.write_text(json.dumps(held, indent=2) + "\n")


if __name__ == "__main__":
    sys.exit(main())
