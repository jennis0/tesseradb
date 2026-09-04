"""How much of the layer's drawn area is one artifact's shape lying on top of another's.

The complaint the concave shape was built to answer says a convex wrap "overlaps every sibling".
That is measurable. The hdbscan layer is a tree, so a parent legitimately covers its children;
what is not legitimate is two artifacts on **different branches** covering the same ground, because
no member is in both. Pairs in an ancestor–descendant relation are excluded.

    python3 overlap.py
"""

import os
import pickle
import sys

import pyarrow.parquet as pq
from shapely.strtree import STRtree

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from load import DATA  # noqa: E402
from measure import to_shapely  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
FAMILIES = ["convex", "dig", "chi", "alpha_complex"]


def ancestry():
    t = pq.read_table(os.path.join(DATA, "clusters-hdbscan.parquet"), columns=["key", "parent"])
    parent = {
        str(k): (str(p) if p is not None else None)
        for k, p in zip(t["key"].to_pylist(), t["parent"].to_pylist())
    }
    chain = {}
    for k in parent:
        seen, cur = set(), parent.get(k)
        while cur:
            seen.add(cur)
            cur = parent.get(cur)
        chain[k] = seen
    return chain


def main():
    # The pickle is this probe's own output, written by `measure.py` in this directory.
    with open(os.path.join(HERE, "rings-hdbscan.pkl"), "rb") as fh:
        rings_by_key = pickle.load(fh)
    chain = ancestry()
    keys = [k for k in rings_by_key if k != "hdb-2422486"]  # the root covers the whole map

    print("| family | drawn area | area covered by 2+ unrelated shapes | share | unrelated pairs overlapping |")
    print("|---|---|---|---|---|")
    for family in FAMILIES:
        geoms, live = [], []
        for k in keys:
            g = to_shapely(rings_by_key[k][family])
            if g is not None and g.area > 0:
                geoms.append(g)
                live.append(k)
        tree = STRtree(geoms)
        total = sum(g.area for g in geoms)
        overlap_area, pairs = 0.0, 0
        for i, g in enumerate(geoms):
            for j in tree.query(g):
                j = int(j)
                if j <= i:
                    continue
                if live[j] in chain[live[i]] or live[i] in chain[live[j]]:
                    continue
                inter = g.intersection(geoms[j])
                if inter.area > 0:
                    overlap_area += inter.area
                    pairs += 1
        print(
            f"| {family} | {total:.4g} | {overlap_area:.4g} | {100 * overlap_area / total:.2f}% "
            f"| {pairs} |"
        )


if __name__ == "__main__":
    main()
