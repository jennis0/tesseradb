"""Simplification is not free: what Douglas–Peucker costs in containment.

Douglas–Peucker keeps a **subset** of the input vertices, so a simplified χ-shape still has only
members for vertices — the property that decides whether a family is admissible at all. What it
does not keep is containment: cutting a corner moves the boundary inwards past the members that
corner enclosed. This measures how far.

    python3 dp_sweep.py
"""

import json
import os
import pickle
import sys

import numpy as np
from matplotlib.path import Path as MplPath

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from load import Layer  # noqa: E402
from measure import ON_BOUNDARY, to_shapely  # noqa: E402
from shapes import simplify_ring  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
DIVISORS = [32, 16, 8, 4, 2]


def main():
    layer = Layer("hdbscan")
    # The pickle is this probe's own output, written by `measure.py` in this directory.
    with open(os.path.join(HERE, "rings-hdbscan.pkl"), "rb") as fh:
        rings_by_key = pickle.load(fh)
    with open(os.path.join(HERE, "results-hdbscan.json")) as fh:
        base = {r["key"]: r for r in json.load(fh)}

    totals = {d: {"vertices": 0, "outside": 0, "area": []} for d in DIVISORS}
    members_total = 0
    for i, key in enumerate(layer.keys):
        p = np.unique(layer.members(i).astype(np.float64), axis=0)
        members_total += len(p)
        chi = rings_by_key[key]["chi"]
        alpha = base[key]["alpha"]
        chi_area = to_shapely(chi).area
        for d in DIVISORS:
            simple = simplify_ring(chi, alpha / d)
            path = MplPath(np.vstack([simple[0], simple[0][:1]]))
            inside = int(path.contains_points(p, radius=ON_BOUNDARY).sum())
            totals[d]["vertices"] += sum(len(r) for r in simple)
            totals[d]["outside"] += len(p) - inside
            g = to_shapely(simple)
            totals[d]["area"].append((g.area / chi_area) if (g is not None and chi_area) else 1.0)
        if (i + 1) % 40 == 0:
            print(f"  {i + 1}/{len(layer)}", file=sys.stderr)

    chi_v = sum(r["chi"]["vertices"] for r in base.values())
    print(f"\nchi-shape unsimplified: {chi_v:,} vertices, {8 * chi_v:,} wire bytes, 0 members outside\n")
    print("| Douglas-Peucker tolerance | vertices | wire bytes | area / chi (median) | members outside | share of member rows |")
    print("|---|---|---|---|---|---|")
    for d in DIVISORS:
        t = totals[d]
        print(
            f"| alpha/{d} | {t['vertices']:,} | {8 * t['vertices']:,} "
            f"| {np.median(t['area']):.3f} | {t['outside']:,} "
            f"| {100 * t['outside'] / members_total:.2f}% |"
        )


if __name__ == "__main__":
    main()
