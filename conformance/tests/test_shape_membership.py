"""**R2: a shape's membership is exact, and it stays exact across a flush and a fold** —
`polygon-membership.md` §12's conformance obligation, black-box, over the real binary.

R2 fails silently when membership is wrong while every served number stays self-consistent, and
the two places that can happen are bookkeeping and ties, not the test itself: a segment resolved
twice or not at all, a row-base slip, a form built under one generation and read under another;
and a tie mishandled at the boundary. Neither is visible from inside the service, so this module is
a second reader — a plain even-odd walk over the quantised source geometry with a point on an edge
inside, sharing no code with the engine — compared against every served count and every served
point's `membership:<layer>` under three principals, at the build, after a flush that adds points
and after a fold that renumbers every row.

**The fixture is built to hit ties.** Points sit on a 1,024-unit lattice — every one exactly on a
tile line at depth 6 and below, many exactly on a polygon's vertex or edge — beside a random
scatter; the polygons' vertices sit on depth-4 tile corners, their axis-aligned edges lie on tile
lines, one hole touches its outer ring, and two parts of one multipolygon share an edge. The extent
is `[0, 65536]` so an integer coordinate quantises to a cell corner exactly, which is what makes
"on the line" a fact rather than a rounding accident.

**The oracle's tie rule is the half-open ray**, not the engine's symbolic perturbation. Off the
boundary every consistent tie rule gives the same even-odd parity, and on the boundary the rule is
*inside* for both — so the two agree everywhere by construction, and an agreement here is between
two constructions rather than a transcription.

shapely is not in the reference venv, so the walk is written out; it is thirty lines.
"""

from __future__ import annotations

import io
import math
import random
import subprocess
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.ipc as ipc
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
EXTENT_MAX = 65536.0
SCALE = 4294967296.0
SEED = 20260829
ID_KEY_HEX = "0f0e0d0c0b0a09080706050403020100"

LATTICE_STEP = 1024
N_RANDOM = 1500
N_INGEST = 200
WHOLE_MAP = [0.0, 0.0, EXTENT_MAX, EXTENT_MAX]

SHAPES = "regions/shapes"
BOXES = "regions/boxes"

#: The polygons, as `(key, parts)` with each part a list of rings and each ring a list of `(x, y)`
#: in extent units. Vertices on depth-4 tile corners (multiples of 4,096), axis-aligned edges on
#: tile lines, a hole touching its outer at `(28672, 4096)`, and two parts sharing `x = 12288`.
POLYGONS: list[tuple[str, list[list[list[tuple[float, float]]]]]] = [
    ("square", [[[(4096, 4096), (20480, 4096), (20480, 20480), (4096, 20480)]]]),
    (
        "holed",
        [
            [
                [(24576, 4096), (40960, 4096), (40960, 20480), (24576, 20480)],
                [(28672, 4096), (32768, 8192), (28672, 12288)],
            ]
        ],
    ),
    (
        "pair",
        [
            [[(4096, 24576), (12288, 24576), (12288, 32768), (4096, 32768)]],
            [[(12288, 24576), (20480, 24576), (20480, 32768), (12288, 32768)]],
        ],
    ),
    ("diamond", [[[(49152, 8192), (57344, 16384), (49152, 24576), (40960, 16384)]]]),
]

#: The boxes: one on tile lines, one at fractional coordinates.
BOXES_DECLARED: list[tuple[str, tuple[float, float, float, float]]] = [
    ("box-a", (40960.0, 40960.0, 49152.0, 49152.0)),
    ("box-b", (50000.5, 40000.25, 60000.75, 45000.5)),
]

PRINCIPALS = [["1"], ["2"], ["1", "2"]]


def wkt_of(parts) -> str:
    def ring(r):
        pts = list(r) + [r[0]]
        return "(" + ", ".join(f"{x:g} {y:g}" for x, y in pts) + ")"

    return "MULTIPOLYGON (" + ", ".join("(" + ", ".join(ring(r) for r in part) + ")" for part in parts) + ")"


# ---- the fixture's points ---------------------------------------------------------------------


def fixture_points() -> list[tuple[int, float, float]]:
    """`(source_id, x, y)` — the lattice first, then the scatter; every coordinate an `f32`."""
    rng = random.Random(SEED)
    out: list[tuple[int, float, float]] = []
    sid = 0
    for x in range(0, int(EXTENT_MAX), LATTICE_STEP):
        for y in range(0, int(EXTENT_MAX), LATTICE_STEP):
            out.append((sid, float(np.float32(x)), float(np.float32(y))))
            sid += 1
    for _ in range(N_RANDOM):
        out.append(
            (
                sid,
                float(np.float32(rng.uniform(0.0, EXTENT_MAX - 1.0))),
                float(np.float32(rng.uniform(0.0, EXTENT_MAX - 1.0))),
            )
        )
        sid += 1
    return out


def ingest_points(base: int) -> list[tuple[int, float, float]]:
    """Points ingested after the build: on edges, at vertices, inside holes, and a scatter."""
    rng = random.Random(SEED + 1)
    planted = [
        (12288.0, 28000.0),  # the shared edge of `pair`
        (4096.0, 10000.0),  # the square's left edge
        (20480.0, 20480.0),  # the square's corner
        (30000.0, 8000.0),  # inside the hole of `holed`
        (28672.0, 4096.0),  # where the hole touches its outer
        (49152.0, 16384.0),  # the diamond's centre
        (53248.0, 12288.0),  # on the diamond's north-east edge (x + y = 65536)
        (40960.0, 40960.0),  # box-a's corner
        (50000.5, 42000.0),  # box-b's left edge
    ]
    out = [(base + i, float(np.float32(x)), float(np.float32(y))) for i, (x, y) in enumerate(planted)]
    for i in range(len(planted), N_INGEST):
        out.append(
            (
                base + i,
                float(np.float32(rng.uniform(0.0, EXTENT_MAX - 1.0))),
                float(np.float32(rng.uniform(0.0, EXTENT_MAX - 1.0))),
            )
        )
    return out


def term_of(source_id: int) -> str:
    return "1" if source_id % 2 == 0 else "2"


# ---- the oracle -------------------------------------------------------------------------------


def q(v: float) -> int:
    """`fixed32` over the extent — the build's own quantisation, operation for operation."""
    scaled = (v - 0.0) / (EXTENT_MAX - 0.0) * SCALE
    floored = math.floor(scaled)
    if floored <= 0:
        return 0
    if floored >= 0xFFFF_FFFF:
        return 0xFFFF_FFFF
    return int(floored)


def on_segment(px, py, ax, ay, bx, by) -> bool:
    if (bx - ax) * (py - ay) - (by - ay) * (px - ax) != 0:
        return False
    return min(ax, bx) <= px <= max(ax, bx) and min(ay, by) <= py <= max(ay, by)


def inside_polygon(px: int, py: int, parts) -> bool:
    """Even-odd over every ring of every part, a point on an edge inside, vertices quantised as
    the build quantises them. Integer arithmetic throughout."""
    parity = False
    for part in parts:
        for ring in part:
            qr = [(q(x), q(y)) for x, y in ring]
            n = len(qr)
            for i in range(n):
                ax, ay = qr[i]
                bx, by = qr[(i + 1) % n]
                if on_segment(px, py, ax, ay, bx, by):
                    return True
                # The half-open rule: an edge counts where exactly one end is above the ray.
                if (ay > py) != (by > py):
                    # x of the edge at y = py, compared without division: the crossing is right
                    # of px iff (bx - ax) * (py - ay) / (by - ay) + ax > px.
                    lhs = (bx - ax) * (py - ay)
                    rhs = (px - ax) * (by - ay)
                    if (lhs > rhs) if (by - ay) > 0 else (lhs < rhs):
                        parity = not parity
    return parity


def inside_box(px: int, py: int, box) -> bool:
    x0, y0, x1, y1 = box
    return q(x0) <= px <= q(x1) and q(y0) <= py <= q(y1)


def containing(x: float, y: float) -> dict[str, set[str]]:
    px, py = q(x), q(y)
    return {
        SHAPES: {key for key, parts in POLYGONS if inside_polygon(px, py, parts)},
        BOXES: {key for key, box in BOXES_DECLARED if inside_box(px, py, box)},
    }


# ---- the bundle -------------------------------------------------------------------------------


def config_toml() -> str:
    polygons = ",\n".join(f'  {{ key = "{k}", wkt = "{wkt_of(p)}" }}' for k, p in POLYGONS)
    boxes = ",\n".join(
        f"  {{ key = \"{k}\", bbox = [{b[0]}, {b[1]}, {b[2]}, {b[3]}] }}" for k, b in BOXES_DECLARED
    )
    return f"""
[sources]
points = "points.parquet"
pairs  = "pairs.parquet"

[defaults]
source = "points"

[[view]]
name             = "{VIEW_ID}"
extent           = {{ min = 0.0, max = {EXTENT_MAX} }}
source           = "points"
point_visibility = {{ source = "pairs", default = "public" }}

[[attribute]]
name   = "fx_key"
type   = "u64"
render = true

[[layer]]
name = "{SHAPES}"
title = "shapes"
views = ["{VIEW_ID}"]
membership = "spatial"
visibility = "public"
artifact_visibility = {{ default = "inherited" }}
require_member_visibility = "none"
hierarchy = {{ kind = "flat", prune_children = false }}
artifacts = [
{polygons}
]

  [layer.shape]
  kind = "polygon"

[[layer]]
name = "{BOXES}"
title = "boxes"
views = ["{VIEW_ID}"]
membership = "spatial"
visibility = "public"
artifact_visibility = {{ default = "inherited" }}
require_member_visibility = "none"
hierarchy = {{ kind = "flat", prune_children = false }}
artifacts = [
{boxes}
]

  [layer.shape]
  kind = "bbox"
"""


def build_bundle(work: Path, points) -> Path:
    ensure_cli_built()
    work.mkdir(parents=True, exist_ok=True)
    pq.write_table(
        pa.table(
            {
                "entity_id": pa.array([p[0] for p in points], type=pa.uint64()),
                "x": pa.array([p[1] for p in points], type=pa.float32()),
                "y": pa.array([p[2] for p in points], type=pa.float32()),
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
    config = work / "shapes.toml"
    config.write_text(config_toml())
    bundle = work / "bundle"
    deployment = write_deployment(work / "tessera.toml", bundle=bundle, schema=config)
    subprocess.run(
        [str(CLI_BIN), "build", "--deployment", str(deployment), "--out", str(bundle), "--mint-external-ids"],
        cwd=REPO_ROOT,
        env=build_env(ID_KEY_HEX),
        check=True,
    )
    return bundle


def ingest_body(points) -> bytes:
    schema = pa.schema(
        [
            pa.field("external_id", pa.binary()),
            pa.field("x", pa.float32()),
            pa.field("y", pa.float32()),
            pa.field("access", pa.utf8()),
            pa.field("fx_key", pa.uint64()),
        ]
    )
    batch = pa.record_batch(
        [
            pa.array([p[0].to_bytes(8, "little") for p in points], pa.binary()),
            pa.array([p[1] for p in points], pa.float32()),
            pa.array([p[2] for p in points], pa.float32()),
            pa.array([term_of(p[0]) for p in points], pa.utf8()),
            pa.array([p[0] for p in points], pa.uint64()),
        ],
        schema=schema,
    )
    sink = io.BytesIO()
    with ipc.new_stream(sink, schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue()


# ---- the comparison ---------------------------------------------------------------------------


def visible(points, terms: list[str]):
    held = set(terms)
    return [p for p in points if term_of(p[0]) in held]


def expected_counts(points, terms: list[str]) -> dict[tuple[str, str], int]:
    counts: dict[tuple[str, str], int] = {}
    for _, x, y in visible(points, terms):
        for layer, keys in containing(x, y).items():
            for key in keys:
                counts[(layer, key)] = counts.get((layer, key), 0) + 1
    return counts


def check_stage(server, points, stage: str) -> None:
    by_fx = {p[0]: (p[1], p[2]) for p in points}
    for terms in PRINCIPALS:
        token = server.authorise(terms)["token"]
        body = server.viewport(token, VIEW_ID, 0, WHOLE_MAP, k=100_000)
        artifacts = decode_viewport_artifacts(body)
        served = {(a.layer, a.key): a for a in artifacts if a.layer in (SHAPES, BOXES)}
        expected = expected_counts(points, terms)
        got = {k: a.masked_count for k, a in served.items()}
        assert got == expected, f"{stage}, principal {terms}: served counts {got} != oracle {expected}"

        # Every served point's membership column, against the shapes that contain it. A flat
        # layer whose shapes overlap on a shared edge has two containing artifacts for a point on
        # it; the column names one of them, and the assertion is that it names one of the oracle's.
        table = decode_viewport_points(body)
        ids_by_key = {(layer, key): a.tessera_id for (layer, key), a in served.items()}
        fx = table.column("fx_key").to_pylist()
        n_visible = len(visible(points, terms))
        assert len(fx) == n_visible, f"{stage}, principal {terms}: {len(fx)} points served of {n_visible} visible"
        for layer in (SHAPES, BOXES):
            column = f"membership:{layer}"
            assert column in table.column_names, f"{stage}: no {column} column in {table.column_names}"
            values = table.column(column).to_pylist()
            wrong = []
            for key, value in zip(fx, values):
                x, y = by_fx[key]
                inside = containing(x, y)[layer]
                allowed = {ids_by_key[(layer, k)] for k in inside if (layer, k) in ids_by_key}
                if inside and value not in allowed:
                    wrong.append((key, (x, y), value, sorted(inside)))
                if not inside and value is not None:
                    wrong.append((key, (x, y), value, []))
            assert not wrong, f"{stage}, principal {terms}, {layer}: {len(wrong)} point(s) disagree; first {wrong[:5]}"


@pytest.fixture(scope="module")
def shapes_server(tmp_path_factory):
    points = fixture_points()
    bundle = build_bundle(tmp_path_factory.mktemp("shape-fixture"), points)
    server, proc = spawn_server(
        bundle,
        tmp_path_factory.mktemp("shape-server"),
        max_k=100_000,
        k_max_marks=100_000,
        theta_target_marks=10**12,
    )
    yield server, points
    stop_server(proc)


def test_membership_is_exact_at_the_build_after_a_flush_and_after_a_fold(shapes_server):
    server, points = shapes_server
    check_stage(server, points, "at the build")

    ingested = ingest_points(len(points))
    resp = server.ingest(ingest_body(ingested), "shapes-1")
    assert resp.status_code in (200, 202), resp.text
    server.flush()
    everything = points + ingested
    check_stage(server, everything, "after a flush")

    server.compact()
    check_stage(server, everything, "after a fold")
