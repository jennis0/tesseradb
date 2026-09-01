"""What the two routes cost, judged on the properties Tessera stores rather than on the pictures.

Three questions, in the order they matter:

1. **Does the projection keep neighbourhoods?** The share of a point's true full-dimension cosine
   neighbours that survive into its 2D neighbourhood. This is the direct test of the premise the
   two views exist to settle — that PCA centres and reprojects, so the geometry UMAP sees is not
   the geometry the encoder produced. If that premise is right, this number separates the routes.
2. **Does it change tile occupancy under Morton order?** The property every measurement in this
   corpus rests on, and the one a bundle actually stores.
3. **Does it scatter a labelled concept?** Reported two ways, because the obvious way is wrong —
   see `category_spread` below.

**Two confounds were found the hard way and both are guarded here.** A per-category spread divided
by its view's own global radius measures how compact the *layout* is, not how coherent the concept
is: at 200,000 it reported PCA-64 scattering every archive by 6% while the scale-free purity
measure showed no difference at all, and the ratio tracked frame use (66.8% against 85.4%) rather
than anything about concepts. And an occupancy count over a frame fitted to the full coordinate
range charges a layout for its outliers, since `extent = "auto"` fits exactly that box — so
occupancy is reported over the fitted frame *and* over a 1st-99th percentile one.

It reads what `prepare.py` wrote — `positions.npy` and `manifest.json` — so the sample it measures
is the sample the run drew, rather than two command lines that have to agree.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np


def morton_cells(xy: np.ndarray, zoom: int, robust: bool = False) -> np.ndarray:
    """Quantise to the unit square over the view's own extent, then interleave. One cell per point.

    `robust` clips the frame to the 1st-99th percentile instead of the full range. The honest
    figure is the default one, `extent = "auto"` fitting the full range; the robust one separates
    "this route uses the grid better" from "the other route threw a few points further".
    """
    lo, hi = (np.percentile(xy, [1, 99], axis=0) if robust else (xy.min(0), xy.max(0)))
    span = np.where(hi - lo == 0, 1, hi - lo)
    n = 1 << zoom
    ij = np.clip(((xy - lo) / span * n).astype(np.int64), 0, n - 1)

    def spread(v):
        v = v.astype(np.uint64)
        for s, m in ((16, 0x0000FFFF0000FFFF), (8, 0x00FF00FF00FF00FF), (4, 0x0F0F0F0F0F0F0F0F),
                     (2, 0x3333333333333333), (1, 0x5555555555555555)):
            v = (v | (v << np.uint64(s))) & np.uint64(m)
        return v

    return spread(ij[:, 0]) | (spread(ij[:, 1]) << np.uint64(1))


def occupancy(xy: np.ndarray, zoom: int, robust: bool = False) -> dict:
    _, counts = np.unique(morton_cells(xy, zoom, robust), return_counts=True)
    return dict(zoom=zoom, distinct_cells=int(len(counts)),
                resolution=float((counts == 1).sum() / len(xy)),
                p50=int(np.percentile(counts, 50)), p99=int(np.percentile(counts, 99)),
                max=int(counts.max()))


def frame_use(xy: np.ndarray) -> float:
    """The share of the fitted box the middle 98% of points occupy. A low number is grid spent on
    outliers, which `extent = "auto"` pays for in full."""
    lo, hi = xy.min(0), xy.max(0)
    p1, p99 = np.percentile(xy, [1, 99], axis=0)
    return float(np.prod((p99 - p1) / (hi - lo)))


def exact_neighbours(X: np.ndarray, q: np.ndarray, k: int) -> np.ndarray:
    """Exact full-dimension cosine neighbours for the query rows — what both routes approximate.

    Brute force on the GPU, and it is *exact* rather than the CAGRA index the kNN route builds: the
    ground truth for a comparison must not be one of the things being compared. Validated against a
    plain NumPy computation at 20,000 x 20,000 — agreement to 2e-6 in distance, the 0.04% of
    neighbour sets that differ being ties between duplicate papers.
    """
    import cupy as cp
    from cuvs.neighbors import brute_force

    d = cp.asarray(X)
    index = brute_force.build(d, metric="cosine")
    nb = cp.empty((len(q), k + 1), dtype=cp.int64)
    ds = cp.empty((len(q), k + 1), dtype=cp.float32)
    brute_force.search(index, cp.asarray(X[q]), k + 1, neighbors=nb, distances=ds)
    cp.cuda.runtime.deviceSynchronize()
    out = cp.asnumpy(nb)[:, 1:]  # drop the point itself
    del d, index, nb, ds
    cp.get_default_memory_pool().free_all_blocks()
    return out


def knn_recall(xy: np.ndarray, q: np.ndarray, true_nb: np.ndarray, k: int) -> float:
    """Of each query's k true full-dimension neighbours, how many are among its k nearest in 2D."""
    from scipy.spatial import cKDTree

    _, got = cKDTree(xy).query(xy[q], k=k + 1)
    return float(np.mean([len(set(a[1:]) & set(b)) / k for a, b in zip(got, true_nb)]))


def category_purity(xy: np.ndarray, cats: np.ndarray, q: np.ndarray, k: int):
    """Of a point's k nearest neighbours **in the view**, what share carry its own category.

    Scale-free by construction, so unlike `category_spread` it cannot be moved by one layout
    throwing its outliers further than the other. **This is the number to quote** for whether a
    route keeps a concept together.
    """
    from scipy.spatial import cKDTree

    _, got = cKDTree(xy).query(xy[q], k=k + 1)
    same = cats[got[:, 1:]] == cats[q][:, None]
    return float(same.mean()), same.mean(axis=1)


def category_spread(xy: np.ndarray, cats: np.ndarray, min_n: int = 200) -> dict:
    """Per-category RMS distance from its own centroid, over the view's global RMS radius.

    ⊘ **Kept for the record, and it is not the measure to quote.** The normaliser makes it read a
    layout's global compactness as a concept's incoherence; at 200,000 it reported PCA-64 spreading
    75% of archives while `category_purity` showed the two routes identical to 0.1 points. Use it
    only beside `frame_use`, which is what it is actually tracking.
    """
    scale = np.sqrt(((xy - xy.mean(0)) ** 2).sum(1).mean())
    out = {}
    for c in np.unique(cats):
        m = cats == c
        if m.sum() < min_n:
            continue
        p = xy[m]
        out[str(c)] = float(np.sqrt(((p - p.mean(0)) ** 2).sum(1).mean()) / scale)
    return out


def main() -> None:
    from ..common.paths import ladder
    from . import sources
    from .prepare import RUNG, SEED

    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--queries", type=int, default=20_000)
    ap.add_argument("--k", type=int, default=15)
    ap.add_argument("--out", type=Path, default=None, help=f"default $TESSERA_LADDER/{RUNG}")
    args = ap.parse_args()

    out = args.out or ladder(RUNG)
    manifest = json.loads((out / "manifest.json").read_text())
    views = tuple(manifest["views"])
    positions = np.load(out / "positions.npy")

    corpus, _ = sources.load_metadata(prose_columns=())
    take = sources.sample_rows(corpus.num_rows, manifest["sample"], SEED)
    assert len(take) == positions.shape[1], (
        f"{positions.shape[1]:,} positions against a {len(take):,} sample — "
        f"{out / 'manifest.json'} and {out / 'positions.npy'} came from different runs"
    )
    primary = np.array([(c.split()[0].split(".")[0] if c else "")
                        for c in np.asarray(corpus.column("categories"))[take]])
    X = sources.load_embeddings(corpus, take)

    q = np.sort(np.random.default_rng(0).choice(len(take), min(args.queries, len(take)), replace=False))
    print(f"exact full-dimension neighbours for {len(q):,} queries...", flush=True)
    true_nb = exact_neighbours(X, q, args.k)
    del X

    report = {"n": len(take), "k": args.k, "queries": len(q), "views": {}}
    for i, view in enumerate(views):
        xy = positions[i]
        purity, per_point = category_purity(xy, primary, q, args.k)
        report["views"][view] = {
            "knn_recall_2d": knn_recall(xy, q, true_nb, args.k),
            "category_purity": purity,
            "frame_use": frame_use(xy),
            "occupancy": [occupancy(xy, z) for z in (8, 12, 16)],
            "occupancy_robust": [occupancy(xy, z, robust=True) for z in (8, 12, 16)],
            "purity_by_category": {str(c): float(per_point[primary[q] == c].mean())
                                   for c in np.unique(primary[q]) if (primary[q] == c).sum() >= 100},
            "spread": category_spread(xy, primary),
        }
        print(f"  {view}: 2D recall@{args.k} {100*report['views'][view]['knn_recall_2d']:.2f}%  "
              f"purity {100*purity:.2f}%", flush=True)

    (out / "compare.json").write_text(json.dumps(report, indent=2))
    v = report["views"]
    w = max(len(x) for x in views) + 2
    print(f"\n{'':14}" + "".join(f"{x:>{w+4}}" for x in views))
    for label, key in (("2D recall", "knn_recall_2d"), ("purity", "category_purity"),
                       ("frame use", "frame_use")):
        print(f"{label:14}" + "".join(f"{100*v[x][key]:>{w+3}.2f}%" for x in views))
    for i, z in enumerate((8, 12, 16)):
        print(f"z{z:<13}" + "".join(f"{v[x]['occupancy'][i]['distinct_cells']:>{w+4},}" for x in views)
              + "   distinct cells, fitted frame")
        print(f"{'':14}" + "".join(f"{v[x]['occupancy_robust'][i]['distinct_cells']:>{w+4},}" for x in views)
              + "   distinct cells, 1-99% frame")
    print(f"\nwritten to {out / 'compare.json'}")


if __name__ == "__main__":
    main()
