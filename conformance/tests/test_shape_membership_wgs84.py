"""**A shape declared in longitude and latitude selects its curved projected image's rows** —
`polygon-membership.md` §4.3 and R10, `projections.md` §10, black-box over the real binary.

The space a polygon is declared in defines the plane its edges are straight in. An edge written in
degrees is straight in the longitude/latitude plane, so its image in a Web Mercator frame is a
**curve**, and the straight chord between the two projected endpoints is a different boundary
selecting different rows. This module is the second reader for that claim: it projects the points
itself, densifies each declared edge itself, walks even-odd over the result itself, and compares
masked counts and every served point's `membership:<layer>` against the service.

**It also computes the reading the owner ruled against** — the vertices projected and joined with
straight lines — and asserts the service does *not* agree with it. Without that half, a test could
pass whichever semantics were built: two implementations that both join with chords agree
perfectly. The fixture plants points in the band between the two boundaries so the disagreement is
a number rather than a possibility; on the United Kingdom's diagonal that band is 60 depth-16 cells
wide at its midpoint, 21.5 km on the ground.

**No point sits near a boundary.** Densification is bounded by one depth-16 cell (`polygon-
membership.md` §4.3), so the two readers may legitimately disagree about a position within a cell
of the true edge. Every point here is rejected unless it is at least three cells clear of every
declared boundary, which makes an exact agreement on counts the right assertion rather than an
optimistic one.

⊘ **The build stage only, and no longer for want of a route.** A projected view now takes
`lon`/`lat` at ingest (`projections.md` §3), so the flush and fold stages are reachable for a
`wgs84` shape and simply are not covered here; `test_shape_membership.py` covers them over an
unprojected view.
"""

from __future__ import annotations

import math
import subprocess
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from oracle.harness import (
    CLI_BIN,
    REPO_ROOT,
    build_env,
    ensure_cli_built,
    spawn_server,
    stop_server,
    write_deployment,
)
from oracle.wire import decode_viewport_artifacts, decode_viewport_points

VIEW_ID = "s0"
SCALE = 4294967296.0
SEED = 20260830
ID_KEY_HEX = "0f0e0d0c0b0a09080706050403020100"
LAYER = "regions/wgs84"

#: Web Mercator's domain cut, and the whole-world frame it makes square (`projections.md` §5.1).
MAX_LAT = 85.0511287798066
#: The frame is `[0, 1]` on both axes whatever the projection (`projections.md` §4).
WHOLE_MAP = [0.0, 0.0, 1.0, 1.0]

PRINCIPALS = [["1"], ["2"], ["1", "2"]]

#: The polygons, in **degrees**, as `(key, ring)`.
#:
#: `uk` is the shape the phase turns on: its 8°W 50°N → 2°E 58°N edge spans both axes, and its two
#: other edges are a meridian and a parallel, straight in both planes — so the diagonal is the only
#: edge whose readings can differ. `iberia` is all meridians and parallels, where the two readings
#: agree exactly and the comparison is an anchor. `aegean` puts a second curved edge at a different
#: latitude, where the transform's steepness differs.
POLYGONS: list[tuple[str, list[tuple[float, float]]]] = [
    ("uk", [(-8.0, 50.0), (2.0, 58.0), (-8.0, 58.0)]),
    ("iberia", [(-9.0, 37.0), (-1.0, 37.0), (-1.0, 43.0), (-9.0, 43.0)]),
    ("aegean", [(20.0, 34.0), (30.0, 42.0), (20.0, 42.0)]),
]

#: Segments each declared edge is cut into for the oracle's own image of it.
#:
#: Independent of the service's adaptive rule, and far finer: a chord's departure from the curve
#: falls with the square of the subdivision, so at 512 the oracle's boundary is within 3×10⁻⁴ of a
#: depth-16 cell of the true image — four orders inside the one cell the service is allowed.
ORACLE_SEGMENTS = 512

#: How far every fixture point must be from every declared boundary, in depth-16 cells.
CLEARANCE_CELLS = 3.0


# ---- the transform, written out ---------------------------------------------------------------


def project(lon: float, lat: float) -> tuple[float, float]:
    """`projections.md` §5.1, in four lines: the unit square, x east and **y south**."""
    lat = max(-MAX_LAT, min(MAX_LAT, lat))
    merc = math.log(math.tan(math.pi / 4 + math.radians(lat) / 2))
    x = (lon + 180.0) / 360.0
    y = 0.5 - merc / (2 * math.pi)
    return (min(max(x, 0.0), 1.0), min(max(y, 0.0), 1.0))


def q(v: float) -> int:
    """`fixed32` over the `[0, 1]` frame — the build's own quantisation, operation for operation."""
    floored = math.floor(v * SCALE)
    if floored <= 0:
        return 0
    if floored >= 0xFFFF_FFFF:
        return 0xFFFF_FFFF
    return int(floored)


def curved_ring(ring: list[tuple[float, float]]) -> list[tuple[int, int]]:
    """The declared ring's own projected image, in grid units: every edge subdivided in the
    longitude/latitude plane, where it is straight, and each sample projected."""
    out: list[tuple[int, int]] = []
    n = len(ring)
    for i in range(n):
        ax, ay = ring[i]
        bx, by = ring[(i + 1) % n]
        for k in range(ORACLE_SEGMENTS):
            t = k / ORACLE_SEGMENTS
            x, y = project(ax + (bx - ax) * t, ay + (by - ay) * t)
            out.append((q(x), q(y)))
    return out


def chorded_ring(ring: list[tuple[float, float]]) -> list[tuple[int, int]]:
    """The reading the owner ruled against: the vertices projected, joined with straight lines."""
    return [tuple(q(v) for v in project(lon, lat)) for lon, lat in ring]


# ---- the oracle's even-odd walk ---------------------------------------------------------------


def on_segment(px, py, ax, ay, bx, by) -> bool:
    if (bx - ax) * (py - ay) - (by - ay) * (px - ax) != 0:
        return False
    return min(ax, bx) <= px <= max(ax, bx) and min(ay, by) <= py <= max(ay, by)


def inside(px: int, py: int, qr: list[tuple[int, int]]) -> bool:
    """Even-odd over one ring, a point on an edge inside, integer arithmetic throughout — the same
    walk `test_shape_membership.py` uses, over a ring this module built."""
    parity = False
    n = len(qr)
    for i in range(n):
        ax, ay = qr[i]
        bx, by = qr[(i + 1) % n]
        if on_segment(px, py, ax, ay, bx, by):
            return True
        if (ay > py) != (by > py):
            lhs = (bx - ax) * (py - ay)
            rhs = (px - ax) * (by - ay)
            if (lhs > rhs) if (by - ay) > 0 else (lhs < rhs):
                parity = not parity
    return parity


def containing(rings: dict[str, list[tuple[int, int]]], lon: float, lat: float) -> set[str]:
    """The shapes holding a place — the place put through the same transform the corpus was."""
    x, y = project(lon, lat)
    px, py = q(x), q(y)
    return {key for key, ring in rings.items() if inside(px, py, ring)}


# ---- the fixture's points ---------------------------------------------------------------------


def clearance(rings: dict[str, list[tuple[int, int]]], lons, lats) -> np.ndarray:
    """Each point's distance, in depth-16 cells, to the nearest boundary of any declared shape."""
    px = np.array([q(project(lon, lat)[0]) for lon, lat in zip(lons, lats)], dtype=np.float64)
    py = np.array([q(project(lon, lat)[1]) for lon, lat in zip(lons, lats)], dtype=np.float64)
    best = np.full(px.shape, np.inf)
    for ring in rings.values():
        pts = np.array(ring, dtype=np.float64)
        a = pts
        b = np.roll(pts, -1, axis=0)
        d = b - a
        len2 = (d * d).sum(axis=1)
        len2[len2 == 0.0] = 1.0
        # (points, segments) — a few thousand by a few thousand, which numpy does in one pass.
        wx = px[:, None] - a[None, :, 0]
        wy = py[:, None] - a[None, :, 1]
        t = np.clip((wx * d[None, :, 0] + wy * d[None, :, 1]) / len2[None, :], 0.0, 1.0)
        ex = wx - t * d[None, :, 0]
        ey = wy - t * d[None, :, 1]
        best = np.minimum(best, np.sqrt(ex * ex + ey * ey).min(axis=1))
    return best / 65536.0


def planted() -> list[tuple[float, float]]:
    """Places in the band between `uk`'s diagonal and the chord across it.

    At 3°W the edge's own latitude is 54.000° and the chord crosses that meridian at 54.193°;
    everything between is inside the triangle as declared and outside it under the chord reading,
    which has drawn its boundary tens of cells too far north. The four here are at the band's
    middle, where it is 20 to 60 cells wide.
    """
    return [(-3.0, 54.096), (-1.0, 55.681), (-6.0, 51.662), (-5.0, 52.482)]


def fixture_points() -> list[tuple[int, float, float]]:
    """`(source_id, lon, lat)` — the planted band first, then a lattice and a scatter, each point
    at least [`CLEARANCE_CELLS`] from every boundary."""
    rings = {key: curved_ring(ring) for key, ring in POLYGONS}
    candidates: list[tuple[float, float]] = list(planted())
    for i in range(-20, 34):
        for j in range(32, 66):
            candidates.append((float(i) + 0.37, float(j) + 0.41))
    rng = np.random.default_rng(SEED)
    candidates.extend(
        zip(
            rng.uniform(-25.0, 35.0, 400).tolist(),
            rng.uniform(30.0, 68.0, 400).tolist(),
        )
    )
    lons = [c[0] for c in candidates]
    lats = [c[1] for c in candidates]
    clear = clearance(rings, lons, lats)
    for i in range(len(planted())):
        assert clear[i] >= CLEARANCE_CELLS, (
            f"the planted point {candidates[i]} is {clear[i]} cells from a boundary — the fixture "
            "cannot distinguish the two readings there"
        )
    kept = [
        (lons[i], lats[i]) for i in range(len(candidates)) if clear[i] >= CLEARANCE_CELLS
    ]
    return [(sid, lon, lat) for sid, (lon, lat) in enumerate(kept)]


def term_of(source_id: int) -> str:
    return "1" if source_id % 2 == 0 else "2"


# ---- the bundle -------------------------------------------------------------------------------


def wkt_of(ring: list[tuple[float, float]]) -> str:
    pts = list(ring) + [ring[0]]
    return "POLYGON ((" + ", ".join(f"{x:g} {y:g}" for x, y in pts) + "))"


def config_toml() -> str:
    artifacts = ",\n".join(
        f'  {{ key = "{k}", wkt = "{wkt_of(r)}", space = "wgs84" }}' for k, r in POLYGONS
    )
    return f"""
[sources]
points = "points.parquet"
pairs  = "pairs.parquet"

[defaults]
source = "points"

[[view]]
name             = "{VIEW_ID}"
projection       = "web_mercator"
extent           = {{ lon = [-180.0, 180.0], lat = [-{MAX_LAT}, {MAX_LAT}] }}
source           = "points"
point_visibility = {{ source = "pairs", default = "public" }}

[[attribute]]
name   = "fx_key"
type   = "u64"
render = true

[[layer]]
name = "{LAYER}"
title = "wgs84 regions"
views = ["{VIEW_ID}"]
membership = "spatial"
visibility = "public"
artifact_visibility = {{ default = "inherited" }}
require_member_visibility = "none"
hierarchy = {{ kind = "flat", prune_children = false }}
artifacts = [
{artifacts}
]

  [layer.shape]
  kind = "polygon"
"""


def build_bundle(work: Path, points) -> Path:
    ensure_cli_built()
    work.mkdir(parents=True, exist_ok=True)
    pq.write_table(
        pa.table(
            {
                "entity_id": pa.array([p[0] for p in points], type=pa.uint64()),
                "lon": pa.array([p[1] for p in points], type=pa.float64()),
                "lat": pa.array([p[2] for p in points], type=pa.float64()),
                "fx_key": pa.array([p[0] for p in points], type=pa.uint64()),
            }
        ),
        work / "points.parquet",
    )
    pq.write_table(
        pa.table(
            {
                "entity_id": pa.array([p[0] for p in points], type=pa.uint64()),
                "term_id": pa.array([int(term_of(p[0])) for p in points], type=pa.uint32()),
            }
        ),
        work / "pairs.parquet",
    )
    config = work / "wgs84-shapes.toml"
    config.write_text(config_toml())
    bundle = work / "bundle"
    deployment = write_deployment(work / "tessera.toml", bundle=bundle, schema=config)
    subprocess.run(
        [
            str(CLI_BIN),
            "build",
            "--deployment",
            str(deployment),
            "--out",
            str(bundle),
            "--mint-external-ids",
        ],
        cwd=REPO_ROOT,
        env=build_env(ID_KEY_HEX),
        check=True,
    )
    return bundle


# ---- the comparison ---------------------------------------------------------------------------


def visible(points, terms: list[str]):
    held = set(terms)
    return [p for p in points if term_of(p[0]) in held]


def counts(rings, points, terms) -> dict[str, int]:
    out: dict[str, int] = {}
    for _, lon, lat in visible(points, terms):
        for key in containing(rings, lon, lat):
            out[key] = out.get(key, 0) + 1
    return out


@pytest.fixture(scope="module")
def wgs84_server(tmp_path_factory):
    points = fixture_points()
    bundle = build_bundle(tmp_path_factory.mktemp("wgs84-fixture"), points)
    server, proc = spawn_server(
        bundle,
        tmp_path_factory.mktemp("wgs84-server"),
        max_k=100_000,
        k_max_marks=100_000,
        theta_target_marks=10**12,
    )
    yield server, points
    stop_server(proc)


def test_a_wgs84_shapes_masked_counts_are_its_curved_images(wgs84_server):
    """Masked counts, per principal, against a second reader that projected the boundary itself —
    and *not* against the chord reading, which the same second reader also computes."""
    server, points = wgs84_server
    curved = {key: curved_ring(ring) for key, ring in POLYGONS}
    chorded = {key: chorded_ring(ring) for key, ring in POLYGONS}

    for terms in PRINCIPALS:
        token = server.authorise(terms)["token"]
        body = server.viewport(token, VIEW_ID, 0, WHOLE_MAP, k=100_000)
        served = {
            a.key: a.masked_count
            for a in decode_viewport_artifacts(body)
            if a.layer == LAYER
        }
        assert served == counts(curved, points, terms), f"principal {terms}"

        # The same numbers under the chord reading differ, and differ on the shape with the long
        # diagonal: an implementation that joined the projected vertices with straight lines would
        # match this instead.
        by_chord = counts(chorded, points, terms)
        assert served != by_chord, f"principal {terms}: the two readings agree, so nothing is proven"
        assert served["uk"] > by_chord["uk"], (
            f"principal {terms}: the curved edge holds {served['uk']} rows and the chord "
            f"{by_chord['uk']} — the band between them should belong to the curve"
        )
        # The shape whose every edge is a meridian or a parallel is straight in both planes, and
        # the two readings agree on it exactly. That is the anchor for the ones above.
        assert served["iberia"] == by_chord["iberia"]


def test_every_served_points_membership_is_the_curved_images(wgs84_server):
    """`membership:<layer>` per point, which is the per-row form of the same claim."""
    server, points = wgs84_server
    curved = {key: curved_ring(ring) for key, ring in POLYGONS}
    by_fx = {p[0]: (p[1], p[2]) for p in points}
    column = f"membership:{LAYER}"

    for terms in PRINCIPALS:
        token = server.authorise(terms)["token"]
        body = server.viewport(token, VIEW_ID, 0, WHOLE_MAP, k=100_000)
        ids = {
            a.key: a.tessera_id
            for a in decode_viewport_artifacts(body)
            if a.layer == LAYER
        }
        table = decode_viewport_points(body)
        assert column in table.column_names, table.column_names
        fx = table.column("fx_key").to_pylist()
        assert len(fx) == len(visible(points, terms))
        wrong = []
        for key, value in zip(fx, table.column(column).to_pylist()):
            lon, lat = by_fx[key]
            holds = containing(curved, lon, lat)
            allowed = {ids[k] for k in holds if k in ids}
            if holds and value not in allowed:
                wrong.append((key, (lon, lat), value, sorted(holds)))
            if not holds and value is not None:
                wrong.append((key, (lon, lat), value, []))
        assert not wrong, f"principal {terms}: {len(wrong)} point(s) disagree; first {wrong[:5]}"
