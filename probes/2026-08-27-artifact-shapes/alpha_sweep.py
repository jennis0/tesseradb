"""α is a choice. This sweeps it and reports what each value does to every α-driven family.

The engine fixes α at three times the median edge of the principal's own convex wrap
(`derived.rs`, `BRIDGE_FACTOR`). That constant is the thing to interrogate: it was chosen because
the statistic is scale-free and robust, not because 3 was measured against alternatives. This
measures the alternatives on the real layer.

    python3 alpha_sweep.py [factors...]      # default 1 1.5 2 3 5 8
"""

import json
import os
import sys
import time

import numpy as np
from scipy.spatial import Delaunay

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from load import Layer  # noqa: E402
from measure import modality, to_shapely  # noqa: E402
from shapes import alpha_complex, alpha_dig, chi_shape, convex, rings_area  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))


def main(factors, layer_name="hdbscan"):
    layer = Layer(layer_name)
    out = {str(f): [] for f in factors}
    t0 = time.time()
    for i in range(len(layer)):
        p = np.unique(layer.members(i).astype(np.float64), axis=0)
        wrap = convex(p)[0]
        a_wrap = abs(rings_area([wrap]))
        edges = np.sum((np.roll(wrap, -1, axis=0) - wrap) ** 2, axis=1)
        median_edge = float(np.sqrt(np.sort(edges)[len(edges) // 2]))
        tri = Delaunay(p) if len(p) >= 4 else None
        for f in factors:
            alpha = f * median_edge
            dig, trace = alpha_dig(p, factor=f, trace=True)
            ac, meta = alpha_complex(p, alpha=alpha)
            geom = to_shapely(ac)
            dig_geom = to_shapely(dig)
            chi = chi_shape(p, alpha=alpha, tri=tri)
            chi_geom = to_shapely(chi)
            comp = modality(p, tri, alpha)
            total = comp.sum()
            out[str(f)].append(
                {
                    "key": layer.keys[i],
                    "members": int(len(p)),
                    "dig_vertices": len(dig[0]),
                    "dig_area": (dig_geom.area / a_wrap) if a_wrap else 1.0,
                    "dig_fill": (dig_geom.intersection(geom).area / dig_geom.area)
                    if geom is not None and dig_geom.area
                    else 1.0,
                    "dig_at_budget": bool(trace["at_budget"]),
                    "chi_vertices": len(chi[0]),
                    "chi_area": (chi_geom.area / a_wrap) if (chi_geom is not None and a_wrap) else 1.0,
                    "chi_fill": (chi_geom.intersection(geom).area / chi_geom.area)
                    if (geom is not None and chi_geom is not None and chi_geom.area)
                    else 1.0,
                    "ac_rings": len(ac),
                    "ac_holes": meta["holes"],
                    "components_over_5pct": int((comp >= 0.05 * total).sum()),
                }
            )
        del tri
        if (i + 1) % 20 == 0:
            print(f"  {i + 1}/{len(layer)} ({time.time() - t0:.0f}s)", file=sys.stderr)

    with open(os.path.join(HERE, f"results-alpha-sweep-{layer_name}.json"), "w") as fh:
        json.dump(out, fh, indent=1)

    print("\n| alpha (x median wrap edge) | dig vertices | dig area/wrap | dig fill | dig at budget | chi vertices | chi area/wrap | chi fill | alpha-cplx rings > 1 | 2+ components >= 5% |")
    print("|---|---|---|---|---|---|---|---|---|---|")
    for f in factors:
        rows = out[str(f)]
        n = len(rows)
        print(
            f"| {f} "
            f"| {sum(r['dig_vertices'] for r in rows):,} "
            f"| {np.median([r['dig_area'] for r in rows]):.3f} "
            f"| {np.median([r['dig_fill'] for r in rows]):.3f} "
            f"| {sum(r['dig_at_budget'] for r in rows)} / {n} "
            f"| {sum(r['chi_vertices'] for r in rows):,} "
            f"| {np.median([r['chi_area'] for r in rows]):.3f} "
            f"| {np.median([r['chi_fill'] for r in rows]):.3f} "
            f"| {sum(1 for r in rows if r['ac_rings'] > 1)} / {n} "
            f"| {sum(1 for r in rows if r['components_over_5pct'] >= 2)} / {n} |"
        )


if __name__ == "__main__":
    fs = [float(a) for a in sys.argv[1:]] or [1.0, 1.5, 2.0, 3.0, 5.0, 8.0]
    main(fs)
