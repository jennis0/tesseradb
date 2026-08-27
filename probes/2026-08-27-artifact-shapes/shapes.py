"""The candidate shape families, over a set of member positions on the 32-bit grid.

Every function here takes an `(n, 2)` array of grid positions and returns a **list of rings**, each
ring an `(m, 2)` array of vertices in order. A one-ring result is what the wire carries today
(`contracts.md` §3.2: `hull_x`, `hull_y`, two same-length `list<uint32>`); a list longer than one is
what a multi-ring wire would have to carry, and is reported as such rather than flattened.

`alpha_dig` is a **reimplementation of the engine's construction**
(`crates/tessera-engine/src/derived.rs`), not a call into it. Two differences are deliberate and
stated so a reader does not mistake this for the engine:

- The engine's arithmetic is exact in `i128`; this uses `float64` for the vectorised passes, whose
  53-bit mantissa cannot represent every product of two 32-bit coordinates. Ties can therefore
  break differently. `validate.py` checks the reproduction against the figures the engine's own
  measurement published for the same layer.
- The engine buckets the members and prunes; this scans them, because numpy makes the scan fast
  enough and the pruning is not what is under study.
"""

import numpy as np
from scipy.spatial import ConvexHull, Delaunay

DIG_BUDGET = 64
BRIDGE_FACTOR = 3


# ---------------------------------------------------------------- shared helpers


def dedup(points):
    """Sorted, unique positions — what the engine's `concave_hull` starts from."""
    return np.unique(np.asarray(points, dtype=np.float64), axis=0)


def convex(points):
    """The convex wrap, counter-clockwise, collinear vertices dropped."""
    p = dedup(points)
    if len(p) <= 2:
        return [p]
    try:
        h = ConvexHull(p)
    except Exception:
        return [p[:2]]
    return [p[h.vertices]]


def ring_area(ring):
    """Twice the signed area, halved — positive for counter-clockwise."""
    x, y = ring[:, 0], ring[:, 1]
    return 0.5 * float(np.dot(x, np.roll(y, -1)) - np.dot(y, np.roll(x, -1)))


def rings_area(rings):
    """Net area of a ring list, outer rings positive and holes negative by their winding."""
    return sum(ring_area(r) for r in rings)


def normalise_rings(rings):
    """Classify each ring as outer or hole by nesting depth, and set its winding accordingly.

    The boundary walk that produces an α-complex's rings has no inherent orientation, so a ring's
    signed area says nothing about whether it bounds material or a void until the nesting is known.
    A ring nested inside an odd number of others is a hole; outer rings are returned
    counter-clockwise and holes clockwise, which is what a nonzero-winding containment test needs.
    Returns `(rings, n_outer, n_holes)`.
    """
    from matplotlib.path import Path as MplPath

    rings = [r for r in rings if len(r) >= 3]
    if not rings:
        return [], 0, 0
    paths = [MplPath(np.vstack([r, r[:1]])) for r in rings]
    out, outer, holes = [], 0, 0
    for i, r in enumerate(rings):
        depth = sum(
            1 for j, path in enumerate(paths) if j != i and path.contains_point(r[0], radius=0.5)
        )
        want_ccw = depth % 2 == 0
        if want_ccw:
            outer += 1
        else:
            holes += 1
        if (ring_area(r) > 0) != want_ccw:
            r = r[::-1]
        out.append(r)
    return out, outer, holes


def n_vertices(rings):
    return int(sum(len(r) for r in rings))


# ---------------------------------------------------------------- the engine's shape


def _orient(o, a, b):
    return (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])


def _on_segment(p, q, r):
    return (
        _orient(p, q, r) == 0
        and min(p[0], q[0]) <= r[0] <= max(p[0], q[0])
        and min(p[1], q[1]) <= r[1] <= max(p[1], q[1])
    )


def _segments_meet(p1, p2, p3, p4):
    d1, d2 = _orient(p3, p4, p1), _orient(p3, p4, p2)
    d3, d4 = _orient(p1, p2, p3), _orient(p1, p2, p4)
    if ((d1 > 0) != (d2 > 0)) and d1 != 0 and d2 != 0 and ((d3 > 0) != (d4 > 0)) and d3 != 0 and d4 != 0:
        return True
    return (
        (d1 == 0 and _on_segment(p3, p4, p1))
        or (d2 == 0 and _on_segment(p3, p4, p2))
        or (d3 == 0 and _on_segment(p1, p2, p3))
        or (d4 == 0 and _on_segment(p1, p2, p4))
    )


def _nearest_inside(p, a, b):
    """The member closest to the line through `a` and `b`, interior side, projecting inside."""
    dx, dy = b[0] - a[0], b[1] - a[1]
    qx, qy = p[:, 0] - a[0], p[:, 1] - a[1]
    d = dx * qy - dy * qx
    dot_a = dx * qx + dy * qy
    rx, ry = p[:, 0] - b[0], p[:, 1] - b[1]
    dot_b = -dx * rx - dy * ry
    mask = (d >= 0) & (dot_a > 0) & (dot_b > 0)
    if not mask.any():
        return None
    idx = np.flatnonzero(mask)
    dd = d[idx]
    best = dd.min()
    tied = idx[dd == best]
    if len(tied) > 1:  # the engine breaks ties on the position itself, lexicographically
        tied = tied[np.lexsort((p[tied, 1], p[tied, 0]))]
    return p[tied[0]]


def alpha_dig(points, budget=DIG_BUDGET, factor=BRIDGE_FACTOR, trace=False):
    """The shape `main` serves: dig inward from the convex wrap, longest bridging edge first."""
    p = dedup(points)
    hull = convex(p)[0]
    if len(hull) < 3:
        return ([hull], {}) if trace else [hull]

    edges = np.sum((np.roll(hull, -1, axis=0) - hull) ** 2, axis=1)
    alpha_sq = (factor * factor) * float(np.sort(edges)[len(edges) // 2])

    poly = [tuple(v) for v in hull]
    retired = [False] * len(poly)
    limit = len(poly) + budget
    refused = 0

    while len(poly) < limit:
        best_i, best_len = -1, alpha_sq
        for i in range(len(poly)):
            if retired[i]:
                continue
            a, b = poly[i], poly[(i + 1) % len(poly)]
            length = (b[0] - a[0]) ** 2 + (b[1] - a[1]) ** 2
            if length > best_len:
                best_i, best_len = i, length
        if best_i < 0:
            break
        i = best_i
        a, b = poly[i], poly[(i + 1) % len(poly)]
        c = _nearest_inside(p, a, b)
        if c is None or not _admissible(poly, i, tuple(c)):
            retired[i] = True
            refused += 1
            continue
        poly.insert(i + 1, tuple(c))
        retired.insert(i + 1, False)

    ring = np.array(poly, dtype=np.float64)
    if not trace:
        return [ring]
    # A live edge still above α when the loop ends is one the budget did not reach.
    live_over = 0
    for i in range(len(poly)):
        if retired[i]:
            continue
        a, b = poly[i], poly[(i + 1) % len(poly)]
        if (b[0] - a[0]) ** 2 + (b[1] - a[1]) ** 2 > alpha_sq:
            live_over += 1
    return [ring], {
        "alpha": float(np.sqrt(alpha_sq)),
        "wrap_vertices": len(hull),
        "at_budget": len(poly) >= limit,
        "refused_digs": refused,
        "bridges_left": live_over,
    }


def _admissible(poly, i, c):
    n = len(poly)
    a, b = poly[i], poly[(i + 1) % n]
    prev, nxt = (i + n - 1) % n, (i + 1) % n
    for j in range(n):
        if j == i:
            continue
        f0, f1 = poly[j], poly[(j + 1) % n]
        if _on_segment(f0, f1, c):
            return False
        if j == prev:
            if _on_segment(a, c, f0):
                return False
        elif _segments_meet(a, c, f0, f1):
            return False
        if j == nxt:
            if _on_segment(b, c, f1):
                return False
        elif _segments_meet(c, b, f0, f1):
            return False
    return True


# ---------------------------------------------------------------- α-complex proper


def _boundary_rings(edges):
    """Order a set of undirected boundary edges into closed rings.

    Each boundary edge of an α-complex belongs to exactly one kept triangle, so the boundary is a
    union of closed curves and every vertex has even degree. Vertices of degree 4 (two curves
    pinched at a point) are walked by taking the unused edge, which splits the pinch into two rings
    rather than one figure-of-eight.
    """
    adj = {}
    for u, v in edges:
        adj.setdefault(u, []).append(v)
        adj.setdefault(v, []).append(u)
    used = set()
    rings = []
    for start in adj:
        for first in adj[start]:
            key = (min(start, first), max(start, first))
            if key in used:
                continue
            ring = [start]
            used.add(key)
            prev, cur = start, first
            while cur != start:
                ring.append(cur)
                nxt = None
                for cand in adj[cur]:
                    k = (min(cur, cand), max(cur, cand))
                    if cand != prev and k not in used:
                        nxt = cand
                        break
                if nxt is None:
                    break
                used.add((min(cur, nxt), max(cur, nxt)))
                prev, cur = cur, nxt
            if len(ring) >= 3:
                rings.append(ring)
    return rings


def _circumradius_sq(tri):
    a, b, c = tri[:, 0], tri[:, 1], tri[:, 2]
    ab = np.sum((a - b) ** 2, axis=1)
    bc = np.sum((b - c) ** 2, axis=1)
    ca = np.sum((c - a) ** 2, axis=1)
    cross = (b[:, 0] - a[:, 0]) * (c[:, 1] - a[:, 1]) - (b[:, 1] - a[:, 1]) * (c[:, 0] - a[:, 0])
    area4sq = 4.0 * cross * cross
    with np.errstate(divide="ignore", invalid="ignore"):
        r2 = ab * bc * ca / area4sq
    r2[~np.isfinite(r2)] = np.inf
    return r2


def alpha_complex(points, alpha=None, factor=BRIDGE_FACTOR):
    """The textbook α-complex: Delaunay triangles whose circumradius is at most α.

    Disconnected and hole-bearing by construction, which is the point of carrying it. α defaults to
    the same statistic the engine's dig uses, so the two are compared at one length scale.
    """
    p = dedup(points)
    if len(p) < 4:
        return [p], {"components": 1, "holes": 0}
    if alpha is None:
        hull = convex(p)[0]
        edges = np.sum((np.roll(hull, -1, axis=0) - hull) ** 2, axis=1)
        alpha = factor * float(np.sqrt(np.sort(edges)[len(edges) // 2]))
    tri = Delaunay(p)
    simplices = tri.simplices
    keep = _circumradius_sq(p[simplices]) <= alpha * alpha
    kept = simplices[keep]
    if len(kept) == 0:
        return [], {"components": 0, "holes": 0}

    # A boundary edge is one belonging to exactly one kept triangle.
    e = np.concatenate([kept[:, [0, 1]], kept[:, [1, 2]], kept[:, [2, 0]]], axis=0)
    e.sort(axis=1)
    uniq, counts = np.unique(e, axis=0, return_counts=True)
    boundary = uniq[counts == 1]
    rings_idx = _boundary_rings([tuple(x) for x in boundary])
    rings, outer, holes = normalise_rings([p[np.array(r)] for r in rings_idx])
    return rings, {"components": outer, "holes": holes, "alpha": float(alpha)}


# ---------------------------------------------------------------- χ-shape (Duckham et al.)


def chi_shape(points, ell=None, factor=BRIDGE_FACTOR, tri=None, alpha=None):
    """The χ-shape (Duckham, Kulik, Worboys and Galton, 2008): peel the longest boundary edge off
    the Delaunay triangulation while the result stays a **simple, hole-free** polygon.

    A boundary edge longer than `ell` is removed by deleting the one remaining triangle behind it,
    unless that triangle's apex is already on the boundary — the regularity test, which is what
    keeps the result one ring with no holes and no pinch points. Every vertex is a member.

    `ell` defaults to `factor × median convex-wrap edge`, the same α the dig uses, so the families
    are compared at one length scale.
    """
    import heapq

    p = dedup(points)
    if len(p) < 4:
        return [p]
    if ell is None:
        ell = alpha if alpha is not None else _alpha_of(p, factor)
    if tri is None:
        tri = Delaunay(p)
    simplices = tri.simplices
    neighbours = tri.neighbors.copy()

    def edge_of(t, i):
        return int(simplices[t][(i + 1) % 3]), int(simplices[t][(i + 2) % 3])

    boundary = {}  # frozenset edge -> (triangle, index)
    on_boundary = set()
    heap = []
    for t in range(len(simplices)):
        for i in range(3):
            if neighbours[t][i] == -1:
                u, v = edge_of(t, i)
                key = (min(u, v), max(u, v))
                boundary[key] = (t, i)
                on_boundary.update(key)
                heapq.heappush(heap, (-float(np.sum((p[u] - p[v]) ** 2)), key))

    removed = np.zeros(len(simplices), dtype=bool)
    ell_sq = ell * ell
    while heap:
        neg, key = heapq.heappop(heap)
        if key not in boundary:
            continue
        if -neg <= ell_sq:
            break
        t, i = boundary[key]
        if removed[t]:
            continue
        apex = int(simplices[t][i])
        if apex in on_boundary:
            continue  # the regularity test: removing this triangle would pinch or hole the ring
        removed[t] = True
        del boundary[key]
        on_boundary.add(apex)
        for j in range(3):
            if j == i:
                continue
            nb = int(neighbours[t][j])
            u, v = edge_of(t, j)
            nkey = (min(u, v), max(u, v))
            if nb == -1:
                continue
            k = int(np.flatnonzero(neighbours[nb] == t)[0])
            boundary[nkey] = (nb, k)
            heapq.heappush(heap, (-float(np.sum((p[u] - p[v]) ** 2)), nkey))

    rings_idx = _boundary_rings(sorted(boundary.keys()))
    if not rings_idx:
        return convex(p)
    biggest = p[np.array(max(rings_idx, key=len))]
    return [biggest if ring_area(biggest) > 0 else biggest[::-1]]


def _alpha_of(points, factor=BRIDGE_FACTOR):
    """`factor` times the median edge of the convex wrap — the engine's own α, in length units."""
    hull = convex(points)[0]
    if len(hull) < 3:
        return 0.0
    edges = np.sum((np.roll(hull, -1, axis=0) - hull) ** 2, axis=1)
    return factor * float(np.sqrt(np.sort(edges)[len(edges) // 2]))


# ---------------------------------------------------------------- k-nearest-neighbour hull


def knn_hull(points, k=3, cap=800, k_max=128):
    """Moreira and Santos (2007): walk the members, at each step taking the sharpest right turn
    among the `k` nearest unused members, restarting with `k + 1` whenever the walk self-intersects
    or ends up with a member outside.

    Transcribed from the paper's pseudocode. The restart is what enforces containment, and it is
    also what makes the cost unbounded in advance: `k` climbs until the walk succeeds.

    `cap` subsamples — the walk is a Python loop with an O(hull) intersection test per candidate,
    and a shape over a sample is a different object from a shape over the membership
    (decision 0099). This family is drawn, not measured, for that reason.

    **The cap is 800 because the walk does not close above it.** Measured on `hdb-2422544`
    (16,929 members) by `knn_scaling.py`: at 800 sampled members it closes at `k = 64`, and at
    3,000 it dead-ends at every `k` up to 120. `k` is a neighbour *count*, not a length, so it does
    not transfer across memberships of different density.
    """
    from scipy.spatial import cKDTree

    p = dedup(points)
    if len(p) > cap:
        rng = np.random.default_rng(0)
        p = p[np.sort(rng.choice(len(p), cap, replace=False))]
    n = len(p)
    if n < 4:
        return [p]

    tree = cKDTree(p)
    start = int(np.lexsort((p[:, 0], p[:, 1]))[0])
    while k <= min(k_max, n - 1):
        used = np.zeros(n, dtype=bool)
        used[start] = True
        hull = [start]
        current, prev_angle, step = start, 0.0, 2
        ok = True
        while (current != start or step == 2) and used.sum() < n + 1:
            if step == 5:
                used[start] = False
            # Ask for more than k, then keep the k nearest that are still unused.
            want = min(n, k * 4 + 8)
            _, idx = tree.query(p[current], k=want)
            cands = [int(i) for i in np.atleast_1d(idx) if not used[i]][:k]
            if not cands:
                ok = False
                break
            # Sharpest right turn first: clockwise from the direction we arrived on.
            def turn(i):
                d = p[i] - p[current]
                return (prev_angle - np.arctan2(d[1], d[0])) % (2 * np.pi)

            cands.sort(key=turn, reverse=True)
            chosen = None
            for i in cands:
                last = 1 if i == start else 0
                crosses = False
                for j in range(2, len(hull) - last):
                    if _segments_meet(
                        tuple(p[hull[-1]]), tuple(p[i]), tuple(p[hull[-1 - j]]), tuple(p[hull[-j]])
                    ):
                        crosses = True
                        break
                if not crosses:
                    chosen = i
                    break
            if chosen is None:
                ok = False
                break
            hull.append(chosen)
            used[chosen] = True
            d = p[hull[-2]] - p[chosen]
            prev_angle = np.arctan2(d[1], d[0])
            current = chosen
            step += 1
            if current == start:
                break
        if ok and len(hull) >= 4:
            ring = p[np.array(hull[:-1] if hull[-1] == start else hull)]
            from matplotlib.path import Path as MplPath

            path = MplPath(np.vstack([ring, ring[:1]]))
            if path.contains_points(p, radius=0.5).all():
                return [ring if ring_area(ring) > 0 else ring[::-1]]
        k += 1
    return convex(p)


# ---------------------------------------------------------------- shapes that invent vertices


def covariance_ellipse(points, sigma=2.0, steps=48):
    """Mean ± `sigma` standard deviations along the principal axes, as a polygon.

    Every vertex is invented: none is a member's position, and the ellipse both excludes members
    and covers ground no member occupies.
    """
    p = np.asarray(points, dtype=np.float64)
    mu = p.mean(axis=0)
    cov = np.cov(p.T)
    vals, vecs = np.linalg.eigh(cov)
    vals = np.clip(vals, 0, None)
    t = np.linspace(0, 2 * np.pi, steps, endpoint=False)
    circle = np.stack([np.cos(t), np.sin(t)], axis=1)
    return [mu + circle * (sigma * np.sqrt(vals)) @ vecs.T]


def buffered_union(points, radius=None, factor=1.0, simplify=None, cap=1500):
    """The morphological shape: a disc of `radius` around every member, unioned, then simplified.

    Vertices are arcs of the dilating disc, not members.

    `cap` is low because GEOS unions discs pairwise: 500 members take 1.6 s and 2,000 take 34 s.
    A sampled buffered union is enough to show what the family looks like, and the family is
    inadmissible on the vertex test regardless.

    The union is computed in a **local frame** — the members shifted to their own bounding box and
    scaled to a span of 1000 — and mapped back. GEOS is slow to the point of hanging on the raw
    grid coordinates, which run to 4·10⁹ with a radius of 10⁷; the shape is the same up to the
    affine map, and this is a figure-only family in any case.
    """
    from shapely.affinity import affine_transform
    from shapely.geometry import MultiPoint

    p = dedup(points)
    if len(p) > cap:
        rng = np.random.default_rng(0)
        p = p[rng.choice(len(p), cap, replace=False)]
    if radius is None:
        radius = factor * _alpha_of(p)
    lo = p.min(axis=0)
    scale = 1000.0 / max(float(np.max(p.max(axis=0) - lo)), 1.0)
    local = (p - lo) * scale
    geom = MultiPoint([tuple(q) for q in local]).buffer(radius * scale, quad_segs=4)
    if simplify:
        geom = geom.simplify(simplify * scale)
    geom = affine_transform(geom, [1 / scale, 0, 0, 1 / scale, lo[0], lo[1]])
    return _shapely_rings(geom)


def density_level_set(points, resolution=256, quantile=0.5):
    """A contour of a binned density over the members, at the `quantile` level.

    Vertices are marching-squares crossings on a raster, not members.
    """
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    p = np.asarray(points, dtype=np.float64)
    lo, hi = p.min(axis=0), p.max(axis=0)
    span = np.maximum(hi - lo, 1.0)
    h, xe, ye = np.histogram2d(
        p[:, 0], p[:, 1], bins=resolution, range=[[lo[0], lo[0] + span[0]], [lo[1], lo[1] + span[1]]]
    )
    from scipy.ndimage import gaussian_filter

    h = gaussian_filter(h, sigma=2.0)
    occupied = h[h > 0]
    level = float(np.quantile(occupied, quantile)) if len(occupied) else 0.0
    xc = 0.5 * (xe[:-1] + xe[1:])
    yc = 0.5 * (ye[:-1] + ye[1:])
    fig = plt.figure()
    cs = plt.contour(xc, yc, h.T, levels=[level])
    rings = []
    for path in cs.get_paths():
        for poly in path.to_polygons():
            if len(poly) >= 3:
                rings.append(np.asarray(poly))
    plt.close(fig)
    return rings


def simplify_ring(rings, tolerance):
    """Douglas–Peucker over each ring. **Keeps a subset of the input vertices**, so a simplified
    member-vertex shape still has only members for vertices — the one post-process that does."""
    from shapely.geometry import Polygon

    out = []
    for r in rings:
        if len(r) < 4:
            out.append(r)
            continue
        g = Polygon(r).simplify(tolerance, preserve_topology=True)
        out.extend(_shapely_rings(g))
    return out


def chaikin(rings, iterations=2):
    """Corner-cutting smoothing. Every vertex after one pass is a **new** point on an edge."""
    out = []
    for r in rings:
        cur = r
        for _ in range(iterations):
            nxt = []
            for i in range(len(cur)):
                a, b = cur[i], cur[(i + 1) % len(cur)]
                nxt.append(0.75 * a + 0.25 * b)
                nxt.append(0.25 * a + 0.75 * b)
            cur = np.array(nxt)
        out.append(cur)
    return out


def _shapely_rings(geom):
    from shapely.geometry import MultiPolygon, Polygon

    rings = []
    polys = geom.geoms if isinstance(geom, MultiPolygon) else [geom]
    for poly in polys:
        if not isinstance(poly, Polygon) or poly.is_empty:
            continue
        rings.append(np.asarray(poly.exterior.coords)[:-1])
        for hole in poly.interiors:
            rings.append(np.asarray(hole.coords)[:-1])
    return rings
