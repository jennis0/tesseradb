"""The one route to a position at 2.33x10^8 — fit a layout on a staged sample, place the rest off
the share.

Rung 4's route with the one change this rung's disk forces. There, 209 GB of vectors were staged
whole and the placement pass read them off a local memmap; here the matrix is **233,055,986 x 768
float16 = 346 GB** against 217 GB free on this box, so nothing but the fit sample is ever local:

1. **Fit.** `stage.py --fit` writes `staging/fit.f16` — 2,500,000 rows drawn evenly from all 666
   files, L2-normalised, 3.84 GB. That block goes on the card as one CAGRA index, its own kNN graph
   goes to cuML's UMAP as a `precomputed_knn`, and that is the layout.
2. **Place.** A second pass reads each of the 666 files' `emb` column once **off the share**,
   normalises it, searches the same index, and positions every row at the
   **similarity-weighted mean of its 15 fit-set neighbours' positions**. The fit rows are then
   overwritten with their own UMAP positions, so every row goes through one code path. Nothing is
   kept but the 233,055,986 x 2 float32 layout — 1.86 GB.

**The share is the constraint, not the card.** Measured 2026-09-03: one reader sustains ~20 MB/s
off this SMB mount and two or more sustain ~30 MB/s, so the 344 GB pass is ~3.2 hours of I/O
against ~45 minutes of GPU search at rung 4's measured placement rate. `place_from_share` therefore
reads ahead on a small thread pool and hands the card whole row groups; the row-group *order* is
preserved because each batch carries its own global offset and writes its own slice.

**BioCLIP-2 publishes unnormalised embeddings** (norms 35.6 … 69.0 over the first row group,
measured), so both halves normalise explicitly. The metric is still named `cosine` so the index
does its own normalisation too and a change to the staging convention cannot silently change the
graph.

⊘ **The layout is not reproducible under a seed**, for rung 3's and rung 4's reason: CAGRA's index
build is approximate and takes none, so UMAP is handed a different graph each run.

    ~/venvs/projection/bin/python -m test_corpora.treeoflife.routes --rows 2000000

reports index build time, search throughput, the layout's wall and the peak device memory each
stage reached.
"""

from __future__ import annotations

import argparse
import collections
import concurrent.futures as cf
import contextlib
import json
import sys
import threading
import time
from pathlib import Path

import numpy as np

#: One set of UMAP parameters, and `n_neighbors` is also the graph's `k`. Rungs 1, 3 and 4's.
UMAP_PARAMS = dict(n_neighbors=15, min_dist=0.1, n_components=2)

SEED = 0

#: CAGRA's graph degree and its intermediate, and the search's candidate list. The arXiv rung's
#: values; nothing here re-tuned them, the rung being a demonstrator whose recall is not measured.
GRAPH_DEGREE = 32
ITOPK = 128

#: Query rows per search call. 50,000 x 768 float16 is 77 MB on the card, and it is also one source
#: row group — so a batch off the share is a batch through the index with no re-slicing.
BATCH_ROWS = 50_000

#: Row groups read ahead of the card while it searches. Three readers is what the share sustains;
#: the queue is one deeper so a reader never waits on the GPU.
READ_WORKERS = 3
READ_QUEUE = 4


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


def normalise(block: np.ndarray) -> np.ndarray:
    """L2-normalised float16, in place on a float32 working copy."""
    out = np.asarray(block, dtype=np.float32)
    norms = np.linalg.norm(out, axis=1, keepdims=True)
    np.divide(out, np.maximum(norms, 1e-12), out=out)
    return out.astype(np.float16)


# ------------------------------------------------------------------------------- the fit half


def knn_graph(
    block: np.ndarray,
    t: dict,
    *,
    k: int = UMAP_PARAMS["n_neighbors"],
    batch_rows: int = BATCH_ROWS,
) -> tuple[np.ndarray, np.ndarray]:
    """A cosine kNN graph over a resident block, through one CAGRA index.

    Rung 4's sharded merge is gone rather than retained-and-unused: the fit set is one index by
    construction here, and a merge that never runs is a path nothing measured.
    """
    import cupy as cp
    from cuvs.neighbors import cagra

    n = len(block)
    t0 = time.time()
    resident = cp.asarray(block)
    index = cagra.build(
        cagra.IndexParams(metric="cosine", intermediate_graph_degree=2 * GRAPH_DEGREE,
                          graph_degree=GRAPH_DEGREE),
        resident,
    )
    cp.cuda.runtime.deviceSynchronize()
    t["build_seconds"] = round(time.time() - t0, 1)

    best_i = np.zeros((n, k), dtype=np.int32)
    best_d = np.zeros((n, k), dtype=np.float32)
    t1 = time.time()
    params = cagra.SearchParams(itopk_size=ITOPK)
    for lo in range(0, n, batch_rows):
        hi = min(lo + batch_rows, n)
        q = cp.asarray(block[lo:hi])
        nb = cp.empty((hi - lo, k), dtype=cp.uint32)
        ds = cp.empty((hi - lo, k), dtype=cp.float32)
        cagra.search(params, index, q, k, neighbors=nb, distances=ds)
        best_i[lo:hi] = cp.asnumpy(nb.astype(cp.int32))
        best_d[lo:hi] = cp.asnumpy(ds)
        del q, nb, ds
    cp.cuda.runtime.deviceSynchronize()
    search = time.time() - t1
    t["search_seconds"] = round(search, 1)
    print(f"    cagra build {t['build_seconds']:.1f}s, {n:,} queries in {search:.1f}s "
          f"({n / search:,.0f}/s)", flush=True)
    del resident, index
    cp.get_default_memory_pool().free_all_blocks()

    # An approximate index does not always return a row first in its own neighbour list, and a row
    # whose first neighbour is not itself would have UMAP place it on another specimen's position.
    # A photographic corpus carries near-duplicates, so this fires. Repaired rather than trusted.
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


def fit_layout(block: np.ndarray, t: dict, *, managed: bool = False) -> np.ndarray:
    """The fit set's own positions: UMAP over its own CAGRA graph."""
    nbi, nbd = knn_graph(block, t)
    xy = layout(nbi, nbd, t, managed=managed)
    del nbi, nbd
    return xy


# ----------------------------------------------------------------------------- the place half


def share_batches(files, batch_rows: int = BATCH_ROWS, workers: int = READ_WORKERS):
    """`(global offset, float16 normalised block)` per source row group, in file order.

    **The reads run ahead of the card on a small pool and the results are reordered.** Each row
    group is an independent read, so the pool submits `READ_QUEUE + workers` of them at a time and
    yields them in index order; the consumer never sees a batch out of place, and the share is
    never idle while the GPU searches.

    `batch_rows` is what a row group is split into if it is larger; the share's groups are 50,000
    rows, which is the default, so it normally splits nothing.
    """
    import pyarrow.parquet as pq

    # (file index, row group, global offset) for every group, in entity order — from the footers,
    # which are 50 ms apiece over SMB and are the only way to know a group's true row count.
    plan: list[tuple[int, int, int]] = []
    at = 0
    for i, path in enumerate(files):
        meta = pq.ParquetFile(path).metadata
        for g in range(meta.num_row_groups):
            plan.append((i, g, at))
            at += meta.row_group(g).num_rows

    def read(job):
        i, g, offset = job
        table = pq.ParquetFile(files[i]).read_row_group(g, columns=["emb"], use_threads=False)
        values = table.column("emb").combine_chunks()
        block = np.asarray(
            values.values.to_numpy(zero_copy_only=False), dtype=np.float32
        ).reshape(table.num_rows, -1)
        return offset, normalise(block)

    with cf.ThreadPoolExecutor(workers) as pool:
        pending: collections.deque = collections.deque()
        it = iter(plan)
        for job in it:
            pending.append(pool.submit(read, job))
            if len(pending) >= READ_QUEUE + workers:
                break
        while pending:
            offset, block = pending.popleft().result()
            for lo in range(0, len(block), batch_rows):
                yield offset + lo, block[lo : lo + batch_rows]
            del block
            job = next(it, None)
            if job is not None:
                pending.append(pool.submit(read, job))
    return at


def place(batches, fit_block: np.ndarray, fit_xy: np.ndarray, n: int, t: dict) -> np.ndarray:
    """Every row's position, searched against one CAGRA index over the fit set.

    `batches` yields `(global offset, normalised float16 block)`; the positions are written into
    one `(n, 2)` float32 array — 1.86 GB at full scale, and the only thing this pass keeps.
    """
    import cupy as cp
    from cuvs.neighbors import cagra

    k = UMAP_PARAMS["n_neighbors"]
    xy = np.empty((n, 2), dtype=np.float32)
    placed = 0
    t0 = time.time()
    with _step("place", t):
        resident = cp.asarray(fit_block)
        index = cagra.build(
            cagra.IndexParams(metric="cosine", intermediate_graph_degree=2 * GRAPH_DEGREE,
                              graph_degree=GRAPH_DEGREE),
            resident,
        )
        positions = cp.asarray(fit_xy)
        params = cagra.SearchParams(itopk_size=ITOPK)
        for offset, block in batches:
            m = len(block)
            q = cp.asarray(block)
            nb = cp.empty((m, k), dtype=cp.uint32)
            ds = cp.empty((m, k), dtype=cp.float32)
            cagra.search(params, index, q, k, neighbors=nb, distances=ds)
            # Similarity, not distance: a neighbour at distance 0 must dominate the mean, and a
            # cosine distance of 1 is orthogonal and should count for nothing.
            w = cp.clip(1.0 - ds, 0.0, None) + 1e-6
            w /= w.sum(axis=1, keepdims=True)
            got = positions[nb.astype(cp.int32)]
            xy[offset : offset + m] = cp.asnumpy((got * w[:, :, None]).sum(axis=1))
            del q, nb, ds, w, got
            placed += m
            if placed % (BATCH_ROWS * 200) < m:
                wall = time.time() - t0
                print(f"    placed {placed:,}/{n:,} in {wall / 60:.1f} min "
                      f"({placed / wall:,.0f} rows/s, {placed * 1536 / wall / 1e6:.1f} MB/s "
                      f"off the share)", flush=True)
        cp.cuda.runtime.deviceSynchronize()
        del resident, index, positions
        cp.get_default_memory_pool().free_all_blocks()
    t["placed"] = placed
    assert placed == n, f"placed {placed:,} rows against {n:,}"
    return xy


def place_from_share(fit_block, fit_xy, fit_rows, n: int, t: dict, files) -> np.ndarray:
    """The whole corpus's positions: one pass over the share, the fit rows overwritten with their
    own UMAP positions afterwards."""
    xy = place(share_batches(files), fit_block, fit_xy, n, t)
    xy[fit_rows] = fit_xy
    return xy


#: Two views, and only one of them is a route. `bioclip` is the embedding layout this module
#: produces; `geo` is the GBIF join's own longitude and latitude, projected inside the build.
ROUTES = {"bioclip": fit_layout}


# ------------------------------------------------------------------ the measurement driver


def main() -> None:
    ap = argparse.ArgumentParser(description="measure the route before committing a 233M run")
    ap.add_argument("--rows", type=int, default=2_000_000,
                    help="rows of the staged fit sample to fit over; 0 takes all of it")
    ap.add_argument("--managed", action="store_true", help="RMM managed memory for the layout")
    ap.add_argument("--graph-only", action="store_true", help="stop after the kNN graph")
    ap.add_argument("--report", type=Path, default=None, help="append the timings here as JSON")
    args = ap.parse_args()

    from . import sources

    matrix, rows, meta = sources.fit_matrix()
    n = meta["rows"] if not args.rows else min(args.rows, meta["rows"])
    block = np.ascontiguousarray(matrix[:n])
    t: dict = {"rows": n, "fit_staged": meta["rows"], "dim": meta["dim"],
               "managed": args.managed, "measured_at": time.strftime("%Y-%m-%d %H:%M")}
    print(f"{n:,} rows x {meta['dim']} off the staged fit sample", flush=True)

    with DeviceWatermark() as watch:
        with _step("knn", t):
            nbi, nbd = knn_graph(block, t)
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
