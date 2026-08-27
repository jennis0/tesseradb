"""Multi-modality is a property of `membership ∩ M_auth`, not of the layer.

Every figure in `measure.py` is at full membership, which is the **least** modal case a principal
can be served: removing members can split a component and can never join two. So the layer-wide
"3 of 197 artifacts are multi-modal" is a lower bound on what some principal sees, and this
measures how the bound moves as the mask narrows.

The subsets here are **uniform random**, which is the most favourable mask there is: a real mask
follows terms, and terms correlate with position in an embedding, so a real narrow principal will
see a more broken cloud than this does. Read the numbers as a floor on the floor.

α is recomputed from each subset's own convex wrap, exactly as the engine derives it per request.

    python3 mask_sweep.py
"""

import json
import os
import sys

import numpy as np
from scipy.spatial import Delaunay

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from load import Layer  # noqa: E402
from measure import modality, to_shapely  # noqa: E402
from shapes import _alpha_of, alpha_complex, chi_shape  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
SHARES = [1.0, 0.5, 0.2, 0.05, 0.01, 0.002]


def main(n_artifacts=40, seed=0):
    layer = Layer("hdbscan")
    sizes = layer.sizes()
    # A spread over the size range rather than the head: modality is not a size effect, but the
    # small artifacts are where a mask bites first.
    order = np.argsort(sizes)
    picks = order[:: max(1, len(order) // n_artifacts)][:n_artifacts]

    rows = []
    rng = np.random.default_rng(seed)
    for i in picks:
        full = np.unique(layer.members(int(i)).astype(np.float64), axis=0)
        for share in SHARES:
            k = int(round(share * len(full)))
            if k < 16:
                continue
            p = full if share == 1.0 else full[np.sort(rng.choice(len(full), k, replace=False))]
            p = np.unique(p, axis=0)
            alpha = _alpha_of(p)
            tri = Delaunay(p)
            comp = modality(p, tri, alpha)
            total = comp.sum()
            ac, meta = alpha_complex(p, alpha=alpha)
            chi = chi_shape(p, alpha=alpha, tri=tri)
            chi_geom, ac_geom = to_shapely(chi), to_shapely(ac)
            rows.append(
                {
                    "key": layer.keys[int(i)],
                    "share": share,
                    "members": int(len(p)),
                    "over_5pct": int((comp >= 0.05 * total).sum()),
                    "largest_share": float(comp[0] / total),
                    "ac_rings": len(ac),
                    "ac_holes": meta["holes"],
                    "chi_fill": float(chi_geom.intersection(ac_geom).area / chi_geom.area)
                    if (chi_geom is not None and ac_geom is not None and chi_geom.area)
                    else 1.0,
                    "chi_vertices": len(chi[0]),
                }
            )
            del tri

    with open(os.path.join(HERE, "results-mask-sweep.json"), "w") as fh:
        json.dump(rows, fh, indent=1)

    print(f"{len(picks)} artifacts, uniform random masks, alpha recomputed per subset\n")
    print("| mask share | artifacts | median members | 2+ components >= 5% | alpha-complex rings > 1 | median chi fill | median chi vertices |")
    print("|---|---|---|---|---|---|---|")
    for share in SHARES:
        band = [r for r in rows if r["share"] == share]
        if not band:
            continue
        print(
            f"| {share:g} | {len(band)} "
            f"| {int(np.median([r['members'] for r in band])):,} "
            f"| {sum(1 for r in band if r['over_5pct'] >= 2)} / {len(band)} "
            f"| {sum(1 for r in band if r['ac_rings'] > 1)} / {len(band)} "
            f"| {np.median([r['chi_fill'] for r in band]):.3f} "
            f"| {int(np.median([r['chi_vertices'] for r in band]))} |"
        )


if __name__ == "__main__":
    main()
