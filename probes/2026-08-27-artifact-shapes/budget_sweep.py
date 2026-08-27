"""Is the alpha shape's problem the construction, or the budget?

108 of 197 artifacts finish at the 64-vertex budget with a bridging edge still live, so the shape
stops because it ran out of vertices rather than because it ran out of concavity. This asks what
raising the budget buys, which is the cheap alternative to changing the family: it needs no new
dependency and no new code.

Artifacts of 200,000 members and more are excluded: each dig scans every member, so a 1,024-vertex
budget over 2.4M members is 2.5·10⁹ operations in a Python loop. The engine's own dig prunes with a
bucket grid and does not have that cost; this exclusion is the probe's, not the construction's.

Also times the Delaunay triangulation the χ-shape needs, since that is what the χ recommendation
costs and the workspace carries no triangulator.

    python3 budget_sweep.py
"""

import json
import os
import sys
import time

import numpy as np
from scipy.spatial import Delaunay

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from load import Layer  # noqa: E402
from measure import to_shapely  # noqa: E402
from shapes import _alpha_of, alpha_complex, alpha_dig, chi_shape, convex, rings_area  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
BUDGETS = [64, 128, 256, 512, 1024]
CEILING = 200_000


def main():
    layer = Layer("hdbscan")
    rows = []
    t0 = time.time()
    for i in range(len(layer)):
        p = np.unique(layer.members(i).astype(np.float64), axis=0)
        if len(p) >= CEILING:
            continue
        alpha = _alpha_of(p)
        a_wrap = abs(rings_area(convex(p)))

        t = time.time()
        tri = Delaunay(p)
        t_delaunay = time.time() - t
        ac, _ = alpha_complex(p, alpha=alpha)
        ac_geom = to_shapely(ac)
        t = time.time()
        chi = chi_shape(p, alpha=alpha, tri=tri)
        t_chi = time.time() - t
        chi_geom = to_shapely(chi)

        row = {
            "key": layer.keys[i],
            "members": int(len(p)),
            "t_delaunay": t_delaunay,
            "t_chi": t_chi,
            "chi_vertices": len(chi[0]),
            "chi_fill": float(chi_geom.intersection(ac_geom).area / chi_geom.area),
            "chi_area": float(chi_geom.area / a_wrap),
            "budgets": {},
        }
        for b in BUDGETS:
            t = time.time()
            dig, trace = alpha_dig(p, budget=b, trace=True)
            elapsed = time.time() - t
            g = to_shapely(dig)
            row["budgets"][str(b)] = {
                "vertices": len(dig[0]),
                "fill": float(g.intersection(ac_geom).area / g.area),
                "area": float(g.area / a_wrap),
                "at_budget": bool(trace["at_budget"]),
                "seconds": elapsed,
            }
        rows.append(row)
        del tri
        if len(rows) % 20 == 0:
            print(f"  {len(rows)} ({time.time() - t0:.0f}s)", file=sys.stderr)

    with open(os.path.join(HERE, "results-budget-sweep.json"), "w") as fh:
        json.dump(rows, fh, indent=1)

    n = len(rows)
    print(f"\n{n} artifacts under {CEILING:,} members\n")
    print("| dig budget | vertices | wire bytes | area / wrap (median) | fill (median) | at budget | probe seconds |")
    print("|---|---|---|---|---|---|---|")
    for b in BUDGETS:
        d = [r["budgets"][str(b)] for r in rows]
        print(
            f"| {b} | {sum(x['vertices'] for x in d):,} | {8 * sum(x['vertices'] for x in d):,} "
            f"| {np.median([x['area'] for x in d]):.3f} | {np.median([x['fill'] for x in d]):.3f} "
            f"| {sum(x['at_budget'] for x in d)} / {n} | {sum(x['seconds'] for x in d):.1f} |"
        )
    print(
        f"| chi-shape | {sum(r['chi_vertices'] for r in rows):,} "
        f"| {8 * sum(r['chi_vertices'] for r in rows):,} "
        f"| {np.median([r['chi_area'] for r in rows]):.3f} "
        f"| {np.median([r['chi_fill'] for r in rows]):.3f} | — "
        f"| {sum(r['t_chi'] for r in rows):.1f} + {sum(r['t_delaunay'] for r in rows):.1f} Delaunay |"
    )

    print("\n### Where the Delaunay time goes\n")
    print("| members | artifacts | median Delaunay (ms) | median chi peel (ms) | median dig@64 (ms) |")
    print("|---|---|---|---|---|")
    for lo, hi in [(0, 10000), (10000, 50000), (50000, CEILING)]:
        band = [r for r in rows if lo <= r["members"] < hi]
        if not band:
            continue
        print(
            f"| {lo:,} – {hi:,} | {len(band)} "
            f"| {1000 * np.median([r['t_delaunay'] for r in band]):.1f} "
            f"| {1000 * np.median([r['t_chi'] for r in band]):.1f} "
            f"| {1000 * np.median([r['budgets']['64']['seconds'] for r in band]):.1f} |"
        )


if __name__ == "__main__":
    main()
