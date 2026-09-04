"""Does the k-nearest-neighbour concave hull close, and at what k?

The Moreira–Santos walk restarts with `k + 1` whenever it self-intersects, and the paper gives no
bound on where that stops. This measures where it stops on a real cluster, at several sample sizes
— which is the whole question for a service that derives a shape per request per principal, since a
mask changes the density the walk sees.

    python3 knn_scaling.py [artifact-key]
"""

import os
import sys

import numpy as np
from matplotlib.path import Path as MplPath
from scipy.spatial import cKDTree

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from load import Layer  # noqa: E402
from shapes import _segments_meet  # noqa: E402


def walk(p, k):
    """One pass of the walk at a fixed k. Returns `(ring or None, why)`."""
    n = len(p)
    tree = cKDTree(p)
    start = int(np.lexsort((p[:, 0], p[:, 1]))[0])
    used = np.zeros(n, dtype=bool)
    used[start] = True
    hull, current, prev, step = [start], start, 0.0, 2
    while True:
        if step == 5:
            used[start] = False
        _, idx = tree.query(p[current], k=min(n, k * 4 + 8))
        cands = [int(i) for i in np.atleast_1d(idx) if not used[i]][:k]
        if not cands:
            return None, "no candidate"

        def turn(i, cur=current, prev=prev):
            d = p[i] - p[cur]
            return (prev - np.arctan2(d[1], d[0])) % (2 * np.pi)

        cands.sort(key=turn, reverse=True)
        chosen = None
        for i in cands:
            last = 1 if i == start else 0
            crosses = any(
                _segments_meet(
                    tuple(p[hull[-1]]), tuple(p[i]), tuple(p[hull[-1 - j]]), tuple(p[hull[-j]])
                )
                for j in range(2, len(hull) - last)
            )
            if not crosses:
                chosen = i
                break
        if chosen is None:
            return None, f"dead end at {len(hull)} vertices"
        hull.append(chosen)
        used[chosen] = True
        d = p[hull[-2]] - p[chosen]
        prev = np.arctan2(d[1], d[0])
        current = chosen
        step += 1
        if current == start:
            return p[np.array(hull[:-1])], "closed"
        if len(hull) > 2 * n:
            return None, "runaway"


def main(key="hdb-2422544"):
    layer = Layer("hdbscan")
    full = np.unique(layer.members(layer.keys.index(key)).astype(np.float64), axis=0)
    print(f"{key}: {len(full):,} members\n")
    print("| sampled members | k | outcome | vertices | members outside |")
    print("|---|---|---|---|---|")
    for cap in (400, 800, 1500, 3000):
        rng = np.random.default_rng(0)
        p = full[np.sort(rng.choice(len(full), min(cap, len(full)), replace=False))]
        for k in (3, 8, 20, 40, 64, 120):
            ring, why = walk(p, k)
            if ring is None:
                print(f"| {len(p)} | {k} | {why} | — | — |")
                continue
            path = MplPath(np.vstack([ring, ring[:1]]))
            outside = int((~path.contains_points(p, radius=0.5)).sum())
            print(f"| {len(p)} | {k} | closed | {len(ring)} | {outside} |")
            # The algorithm restarts on a member left outside as well as on a self-intersection,
            # so a closed-but-not-containing walk is not an answer.
            if outside == 0:
                break


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "hdb-2422544")
