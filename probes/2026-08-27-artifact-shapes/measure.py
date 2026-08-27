"""The survey: every family over every artifact of the real layer, at full membership.

Nothing here subsamples. A shape over a sample is a different object from a shape over
`membership ∩ M_auth` (decision 0099), and a measurement of the sampled shape would not answer the
question that was asked. The two families that cannot be run over 2.4M members in Python
(`knn_hull`, `buffered_union`) are excluded from this survey for that reason and appear only in
`figures.py`, where they are labelled as sampled.

Metrics, per artifact and per family:

- **vertices** and **wire bytes** — 8 bytes a vertex (`hull_x` and `hull_y`, `uint32` each), plus
  4 bytes an extra ring for the offset a multi-ring wire would need.
- **area**, as a fraction of the convex wrap's.
- **members outside** — the containment property, checked rather than assumed for every family. A
  member is counted inside when it is within half a grid unit of the shape, so a member *on* the
  boundary — every vertex, for the families whose vertices are members — is inside. Half a grid
  unit is 1/2³² of the map: below any distance the client can draw.
- **fill** — the area of the shape that the α-complex at the same α also covers, over the area of
  the shape. The α-complex is the members' own footprint at that scale, so fill is the share of the
  drawn shape that is within reach of a member and `1 − fill` is the void the shape claims. An
  intersection, not a ratio of two areas: a shape that misses the members and covers as much
  elsewhere would score 1 on a ratio and 0 here. Resolution-free, unlike a rasterised occupancy.
- **precision** — of the corpus points inside the shape, the fraction that are members.

Plus, per artifact and independent of any family:

- **modality** — the connected components of the members at the same α the dig uses (two members
  are joined when a chain of members steps between them, each step at most α). The Euclidean
  minimum spanning tree is a subgraph of the Delaunay triangulation, so cutting Delaunay edges
  longer than α gives exactly the single-linkage components at α.

    python3 measure.py            # the hdbscan layer
    python3 measure.py kmeans     # the convex-ish control
"""

import json
import os
import pickle
import sys
import time

import numpy as np
from matplotlib.path import Path as MplPath
from scipy.sparse import coo_matrix
from scipy.sparse.csgraph import connected_components
from scipy.spatial import Delaunay

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from load import Layer, corpus as corpus_positions  # noqa: E402
from shapes import (  # noqa: E402
    _alpha_of,
    alpha_complex,
    alpha_dig,
    chi_shape,
    convex,
    ring_area,
    rings_area,
    simplify_ring,
)

HERE = os.path.dirname(os.path.abspath(__file__))

# Half a grid unit: a member on the boundary counts as inside. See the module doc.
ON_BOUNDARY = 0.5


def compound_path(rings):
    verts, codes = [], []
    for r in rings:
        if len(r) < 3:
            continue
        verts.append(np.vstack([r, r[:1]]))
        codes.append(np.array([MplPath.MOVETO] + [MplPath.LINETO] * (len(r) - 1) + [MplPath.CLOSEPOLY]))
    if not verts:
        return None
    return MplPath(np.vstack(verts), np.concatenate(codes))


def inside_counts(rings, members, corpus):
    """`(members outside the shape, corpus points inside, members inside)`."""
    path = compound_path(rings)
    if path is None:
        return len(members), 0, 0
    stack = np.vstack(rings)
    lo, hi = np.min(stack, axis=0), np.max(stack, axis=0)
    win = corpus[
        (corpus[:, 0] >= lo[0]) & (corpus[:, 0] <= hi[0]) & (corpus[:, 1] >= lo[1]) & (corpus[:, 1] <= hi[1])
    ]
    n_corpus_in = int(path.contains_points(win, radius=ON_BOUNDARY).sum())
    m_in = int(path.contains_points(members, radius=ON_BOUNDARY).sum())
    return len(members) - m_in, n_corpus_in, m_in


def modality(p, tri, alpha):
    """Single-linkage component sizes of the members at α, descending."""
    e = np.concatenate([tri.simplices[:, [0, 1]], tri.simplices[:, [1, 2]], tri.simplices[:, [2, 0]]])
    d2 = np.sum((p[e[:, 0]] - p[e[:, 1]]) ** 2, axis=1)
    keep = e[d2 <= alpha * alpha]
    g = coo_matrix((np.ones(len(keep)), (keep[:, 0], keep[:, 1])), shape=(len(p), len(p)))
    n, labels = connected_components(g, directed=False)
    return np.sort(np.bincount(labels, minlength=n))[::-1]


def wire_bytes(rings):
    v = sum(len(r) for r in rings)
    return 8 * v + 4 * max(0, len(rings) - 1)


def to_shapely(rings):
    """Rings (outer counter-clockwise, holes clockwise) as one shapely geometry."""
    from shapely.geometry import MultiPolygon, Polygon
    from shapely.ops import unary_union

    outers, holes = [], []
    for r in rings:
        if len(r) < 3:
            continue
        (holes if ring_area(r) < 0 else outers).append(r)
    polys = []
    for o in outers:
        poly = Polygon(o)
        inner = [h for h in holes if poly.contains(Polygon(h).representative_point())]
        polys.append(Polygon(o, inner))
    if not polys:
        return None
    geom = MultiPolygon(polys) if len(polys) > 1 else polys[0]
    return geom if geom.is_valid else unary_union(geom.buffer(0))


def family_row(rings, members, corpus, a_wrap, complex_geom):
    outside, corpus_in, m_in = inside_counts(rings, members, corpus)
    geom = to_shapely(rings)
    # The polygon's own area, not the shoelace sum, so that `fill` is a ratio of two areas the same
    # library computed and a nesting subtlety cannot put it above 1.
    area = geom.area if geom is not None else abs(rings_area(rings))
    covered = 0.0
    if geom is not None and complex_geom is not None:
        try:
            covered = geom.intersection(complex_geom).area
        except Exception:
            covered = float("nan")
    return {
        "rings": len(rings),
        "vertices": int(sum(len(r) for r in rings)),
        "bytes": wire_bytes(rings),
        "area_ratio": (area / a_wrap) if a_wrap > 0 else 1.0,
        "fill": (covered / area) if area > 0 else 1.0,
        "members_outside": int(outside),
        "precision": (m_in / corpus_in) if corpus_in else 1.0,
    }


def survey(layer_name="hdbscan", limit=None):
    layer = Layer(layer_name)
    corpus = corpus_positions()
    print(f"corpus reference: {len(corpus):,} distinct positions", file=sys.stderr)

    rows, kept_rings = [], {}
    t0 = time.time()
    order = range(len(layer)) if limit is None else range(min(limit, len(layer)))
    for i in order:
        p = np.unique(layer.members(i).astype(np.float64), axis=0)
        alpha = _alpha_of(p)
        row = {"key": layer.keys[i], "members": int(len(p)), "alpha": alpha}

        tri = Delaunay(p) if len(p) >= 4 else None
        ac, meta = alpha_complex(p, alpha=alpha)
        complex_geom = to_shapely(ac)
        a_complex = complex_geom.area if complex_geom is not None else 0.0

        wrap = convex(p)
        a_wrap = abs(rings_area(wrap))
        row["convex"] = family_row(wrap, p, corpus, a_wrap, complex_geom)

        dig, trace = alpha_dig(p, trace=True)
        row["dig"] = family_row(dig, p, corpus, a_wrap, complex_geom)
        row["dig"].update(
            {
                "at_budget": bool(trace["at_budget"]),
                "bridges_left": int(trace["bridges_left"]),
                "refused_digs": int(trace["refused_digs"]),
            }
        )

        row["alpha_complex"] = family_row(ac, p, corpus, a_wrap, complex_geom)
        row["alpha_complex"].update(
            {
                "outer_rings": meta["components"],
                "holes": meta["holes"],
                "hole_area_share": float(
                    1.0 - a_complex / sum(abs(ring_area(r)) for r in ac if ring_area(r) > 0)
                )
                if ac and any(ring_area(r) > 0 for r in ac)
                else 0.0,
            }
        )

        chi = chi_shape(p, alpha=alpha, tri=tri)
        row["chi"] = family_row(chi, p, corpus, a_wrap, complex_geom)

        # Douglas–Peucker keeps a subset of the input vertices, so a simplified χ-shape still has
        # only members for vertices — but it may cut a corner past a member, so containment is
        # measured rather than assumed.
        chi_dp = simplify_ring(chi, alpha / 2)
        row["chi_dp"] = family_row(chi_dp, p, corpus, a_wrap, complex_geom)

        comp = modality(p, tri, alpha)
        total = int(comp.sum())
        row["modality"] = {
            "components": int(len(comp)),
            "largest_share": float(comp[0] / total),
            "over_1pct": int((comp >= 0.01 * total).sum()),
            "over_5pct": int((comp >= 0.05 * total).sum()),
            "over_10pct": int((comp >= 0.10 * total).sum()),
            "top": [int(c) for c in comp[:6]],
        }
        rows.append(row)
        kept_rings[layer.keys[i]] = {
            "convex": [r.astype(np.float64) for r in wrap],
            "dig": [r.astype(np.float64) for r in dig],
            "alpha_complex": [r.astype(np.float64) for r in ac],
            "chi": [r.astype(np.float64) for r in chi],
            "chi_dp": [r.astype(np.float64) for r in chi_dp],
        }
        del tri
        if len(rows) % 10 == 0:
            print(f"  {len(rows)}/{len(layer)} ({time.time() - t0:.0f}s)", file=sys.stderr)

    with open(os.path.join(HERE, f"results-{layer_name}.json"), "w") as fh:
        json.dump(rows, fh, indent=1)
    with open(os.path.join(HERE, f"rings-{layer_name}.pkl"), "wb") as fh:
        pickle.dump(kept_rings, fh)
    print(f"wrote results-{layer_name}.json in {time.time() - t0:.0f}s", file=sys.stderr)
    return rows


if __name__ == "__main__":
    name = sys.argv[1] if len(sys.argv) > 1 else "hdbscan"
    lim = int(sys.argv[2]) if len(sys.argv) > 2 else None
    survey(name, lim)
