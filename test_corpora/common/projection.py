"""The frozen transform from WGS84 to a view's quantisation frame.

**This module exists to be replaced, and to be checked against its replacement.** Tessera has no
projection layer today (`docs/design/projections.md` §1): a view's `extent` is four numbers and
nothing records what they mean, so a geographic corpus is projected here, before ingest, and the
declaration states the frame the projected numbers live in. When native projection lands, every
`prepare.py` in this tree stops calling this module and emits `lon`/`lat` instead, the declaration
names a projection, and each corpus is rebuilt. That is a rerun rather than a loss: a map
projection is a pure function, which is exactly what `data/geometry.parquet` is not, and is why
that file is hashed rather than seeded (`probes/dataset.md` §3).

Until then this is the reference the Rust has to agree with, so the vectors both are checked against
live in `projection-vectors.json` beside this file — one description that two languages read, rather
than two implementations of one. `python -m test_corpora.common.projection` checks this half of it;
`tessera_spatial::projection` checks the other.

**Two frames, one transform.** `unit` normalises the projection's whole domain to [0, 1] on both
axes; `metres` is the same map without the final scale, in EPSG:3857's own units. They are affine
images of each other, so they quantise to identical cells, and `check_frames_agree` demonstrates
that rather than assuming it. `unit` is the default: a 16-bit cell is then exactly an XYZ tile at
zoom 16, so tile addressing is integer arithmetic and the declaration carries no magic constant.

**y runs south in both.** `clients/ts/core/src/coords.ts` pins, against deck.gl's own `Tileset2D`,
that tile y and cell y increase together, and XYZ tile y = 0 is north — while EPSG:3857's northing
increases *northward*. A frame declared symmetrically in metres is therefore mirrored against every
basemap, and `Bounds::validate` requires `y_max > y_min` so it cannot be repaired by inverting the
extent. The negation is applied here, at the definition, and never left to a caller.
"""

from __future__ import annotations

import json
import math
import pathlib
from typing import Iterable, Literal, NamedTuple

Frame = Literal["unit", "metres"]

#: Semi-major axis of the WGS84 ellipsoid, which Web Mercator uses as a sphere radius.
EARTH_RADIUS_M = 6378137.0

#: Half the projected world, in metres: pi * R. The domain is [-WORLD_HALF_M, +WORLD_HALF_M] on
#: both axes, which is what makes the Web Mercator world square.
WORLD_HALF_M = math.pi * EARTH_RADIUS_M  # 20037508.342789244

#: The latitude at which the projected northing reaches WORLD_HALF_M. Beyond it the world is no
#: longer square, so every tile scheme cuts here; a point past it is *clipped*, not clamped.
MAX_LATITUDE = math.degrees(2.0 * math.atan(math.exp(math.pi)) - math.pi / 2.0)

#: The whole domain, as the WGS84 box a caller would write once `extent` is declared on the input
#: side of the projection (`projections.md` §2). Recorded beside every corpus this module places so
#: that the migration is a substitution rather than a derivation.
WORLD_BOX_WGS84 = {
    "lon": (-180.0, 180.0),
    "lat": (-MAX_LATITUDE, MAX_LATITUDE),
}


class Extent(NamedTuple):
    """A frame, in the units the declaration's `extent = { min, max }` is written in."""

    min: float
    max: float


def extent(frame: Frame = "unit") -> Extent:
    """The whole projected domain, square, in the frame's own units."""
    if frame == "unit":
        return Extent(0.0, 1.0)
    if frame == "metres":
        return Extent(-WORLD_HALF_M, WORLD_HALF_M)
    raise ValueError(f"unknown frame {frame!r}; expected 'unit' or 'metres'")


def clip_latitude(lat: float) -> float:
    """Clip to the projected domain. A point at exactly ±MAX_LATITUDE is inside it."""
    return max(-MAX_LATITUDE, min(MAX_LATITUDE, lat))


def project(lon: float, lat: float, frame: Frame = "unit") -> tuple[float, float]:
    """WGS84 degrees to the frame. x east, **y south**. Latitude is clipped, not wrapped.

    The result is then held inside the frame, which is not the same thing and is not cosmetic.
    `MAX_LATITUDE` is itself the output of `atan` and `exp`, so projecting it returns pi to a
    relative 1e-16 rather than to the bit, and the projected pole lands an ULP *outside* [0, 1].
    The build counts a clamp as `v < min` or `v > max` (`contracts` §2.5), so without this every
    clipped point would be reported as clamped — a real number attributed to the wrong cause, in
    the one report an operator reads to judge whether the frame is right.
    """
    ext = extent(frame)
    lat = clip_latitude(lat)
    merc_y = math.log(math.tan(math.pi / 4.0 + math.radians(lat) / 2.0))
    if frame == "unit":
        x, y = (lon + 180.0) / 360.0, 0.5 - merc_y / (2.0 * math.pi)
    elif frame == "metres":
        x, y = EARTH_RADIUS_M * math.radians(lon), -EARTH_RADIUS_M * merc_y
    else:
        raise ValueError(f"unknown frame {frame!r}; expected 'unit' or 'metres'")
    hold = lambda v: max(ext.min, min(ext.max, v))  # noqa: E731
    return (hold(x), hold(y))


def unproject(x: float, y: float, frame: Frame = "unit") -> tuple[float, float]:
    """The frame back to WGS84 degrees. Exact inverse of `project` for unclipped inputs."""
    if frame == "unit":
        lon = x * 360.0 - 180.0
        merc_y = (0.5 - y) * 2.0 * math.pi
    elif frame == "metres":
        lon = math.degrees(x / EARTH_RADIUS_M)
        merc_y = -y / EARTH_RADIUS_M
    else:
        raise ValueError(f"unknown frame {frame!r}; expected 'unit' or 'metres'")
    return (lon, math.degrees(2.0 * math.atan(math.exp(merc_y)) - math.pi / 2.0))


def sql(lon: str, lat: str, frame: Frame = "unit") -> tuple[str, str]:
    """The same transform as two DuckDB expressions, so 10^7 rows never enter Python.

    `lon` and `lat` are expressions the caller supplies — a column name, or a cast. The latitude
    clip is applied here for the same reason `project` applies it: `tan` diverges at ±90° and
    GeoNames carries points at exactly both poles.
    """
    ext = extent(frame)
    clipped = f"greatest(-{MAX_LATITUDE!r}, least({MAX_LATITUDE!r}, {lat}))"
    merc_y = f"ln(tan(pi() / 4.0 + radians({clipped}) / 2.0))"
    if frame == "unit":
        x, y = f"(({lon}) + 180.0) / 360.0", f"0.5 - ({merc_y}) / (2.0 * pi())"
    elif frame == "metres":
        x, y = (
            f"{EARTH_RADIUS_M!r} * radians({lon})",
            f"-{EARTH_RADIUS_M!r} * ({merc_y})",
        )
    else:
        raise ValueError(f"unknown frame {frame!r}; expected 'unit' or 'metres'")
    hold = lambda v: f"greatest({ext.min!r}, least({ext.max!r}, {v}))"  # noqa: E731
    return (hold(x), hold(y))


def is_clipped(lat: float) -> bool:
    """Whether this latitude falls outside the projected domain and is being moved onto its edge.

    The build's clamp report structurally cannot see these: clipping lands them at *exactly* the
    frame maximum, and `contracts` §2.5 says a point at the maximum is not clamped. So the count is
    taken here and printed by the corpus that takes it (`projections.md` §4a).
    """
    return lat > MAX_LATITUDE or lat < -MAX_LATITUDE


def quantise(v: float, ext: Extent, bits: int = 16) -> int:
    """The engine's own quantisation, for checking that two frames agree. Half-open, clamping."""
    cells = 1 << bits
    t = (v - ext.min) / (ext.max - ext.min)
    return max(0, min(cells - 1, int(t * cells)))


# --------------------------------------------------------------------------------------------
# Test vectors: the contract this module and its Rust replacement both meet. They are data in a
# file beside this one rather than literals here, because two languages have to read them and a
# figure copied into a second language is a figure that can drift from the first.
# --------------------------------------------------------------------------------------------

VECTORS_PATH = pathlib.Path(__file__).with_name("projection-vectors.json")
_VECTORS = json.loads(VECTORS_PATH.read_text(encoding="utf-8"))

#: (lon, lat, x_metres, y_metres) — y south, so the sign is the opposite of EPSG:3857's northing.
#: Every value is exact or a published Web Mercator figure, not one this module produced.
TEST_VECTORS = [
    (v["lon"], v["lat"], v["x_m"], v["y_m"])
    for v in _VECTORS["web_mercator_metres"]["vectors"]
]

#: (name, lon, lat, zoom, tile_x, tile_y) — XYZ tile addresses, which are the y-direction's real
#: test: a mirrored frame passes every round trip and fails these.
TILE_VECTORS = [
    (v["place"], v["lon"], v["lat"], v["zoom"], v["tile_x"], v["tile_y"])
    for v in _VECTORS["xyz_tiles"]["vectors"]
]


def check_frames_agree(samples: Iterable[tuple[float, float]], bits: int = 16) -> int:
    """Quantise each sample through both frames and return how many cells disagree.

    The two frames are affine images of one another, so in exact arithmetic this is zero. It is not
    guaranteed to be zero in floating point, and the coordinate path narrows to `f32`
    (`projections.md` §4a), so it is measured rather than asserted.
    """
    unit_ext, metre_ext = extent("unit"), extent("metres")
    disagreements = 0
    for lon, lat in samples:
        ux, uy = project(lon, lat, "unit")
        mx, my = project(lon, lat, "metres")
        if (quantise(ux, unit_ext, bits), quantise(uy, unit_ext, bits)) != (
            quantise(mx, metre_ext, bits),
            quantise(my, metre_ext, bits),
        ):
            disagreements += 1
    return disagreements


def _self_check() -> None:
    import random

    print(f"MAX_LATITUDE  = {MAX_LATITUDE!r}")
    print(f"WORLD_HALF_M  = {WORLD_HALF_M!r}")
    print(f"world box     = {WORLD_BOX_WGS84}")

    # The shared file records the three constants as figures; this module derives them. Both are
    # checked against the other so the file cannot drift from the arithmetic that produced it —
    # which is the only way the Rust, which reads the file, can rely on it.
    consts = _VECTORS["constants"]
    assert consts["earth_radius_m"] == EARTH_RADIUS_M, consts["earth_radius_m"]
    assert consts["world_half_m"] == WORLD_HALF_M, consts["world_half_m"]
    assert consts["max_latitude_deg"] == MAX_LATITUDE, consts["max_latitude_deg"]
    print(f"vectors file  : {VECTORS_PATH.name}, constants derive to the bit")

    for lon, lat, mx, my in TEST_VECTORS:
        gx, gy = project(lon, lat, "metres")
        assert math.isclose(gx, mx, abs_tol=1e-6), f"x at ({lon}, {lat}): {gx} != {mx}"
        assert math.isclose(gy, my, abs_tol=1e-6), f"y at ({lon}, {lat}): {gy} != {my}"
    print(f"metre vectors : {len(TEST_VECTORS)} exact")

    for lon, lat, _, _ in TEST_VECTORS:
        ux, uy = project(lon, lat, "unit")
        mx, my = project(lon, lat, "metres")
        assert math.isclose(ux, (mx + WORLD_HALF_M) / (2 * WORLD_HALF_M), abs_tol=1e-12)
        assert math.isclose(uy, (my + WORLD_HALF_M) / (2 * WORLD_HALF_M), abs_tol=1e-12)
    print("unit frame    : affine image of metres, both axes")

    for name, lon, lat, z, tx, ty in TILE_VECTORS:
        ux, uy = project(lon, lat, "unit")
        got = (int(ux * (1 << z)), int(uy * (1 << z)))
        assert got == (tx, ty), f"{name} at z{z}: {got} != {(tx, ty)}"
    print(f"tile vectors  : {len(TILE_VECTORS)} correct — y is south")

    worst = 0.0
    for _ in range(200_000):
        lon = random.uniform(-180.0, 180.0)
        lat = random.uniform(-MAX_LATITUDE, MAX_LATITUDE)
        for frame in ("unit", "metres"):
            rl, ra = unproject(*project(lon, lat, frame), frame)
            worst = max(worst, abs(rl - lon), abs(ra - lat))
    print(f"round trip    : 200,000 points, worst error {worst:.3e} degrees")

    samples = [
        (random.uniform(-180.0, 180.0), random.uniform(-MAX_LATITUDE, MAX_LATITUDE))
        for _ in range(200_000)
    ]
    bad = check_frames_agree(samples)
    print(f"frame agreement: {bad} of {len(samples)} samples quantise to a different cell")

    assert is_clipped(85.5) and is_clipped(-90.0) and not is_clipped(MAX_LATITUDE)
    ext = extent("unit")
    north, south = project(0.0, 90.0, "unit")[1], project(0.0, -90.0, "unit")[1]
    cells = 1 << 16
    # The edge is reached to within an ULP rather than exactly: MAX_LATITUDE is itself the output
    # of atan and exp, so the forward transform returns pi to a relative 1e-16 rather than to the
    # bit. What has to hold is the *cell* — a clipped point sits in the frame's first or last cell
    # and is inside the half-open interval, so the build's clamp report reads zero for it, which is
    # why the clip is counted here instead.
    assert north == ext.min and south == ext.max, (north, south)
    assert quantise(north, ext) == 0 and quantise(south, ext) == cells - 1
    for frame in ("unit", "metres"):
        e = extent(frame)
        for lon in (-180.0, 180.0, -181.0, 400.0):
            for lat in (90.0, -90.0, MAX_LATITUDE, -MAX_LATITUDE, 0.0):
                x, y = project(lon, lat, frame)
                assert e.min <= x <= e.max and e.min <= y <= e.max, (frame, lon, lat, x, y)
    print(
        f"clip          : poles land on the frame edge exactly, cells "
        f"{quantise(north, ext)} and {quantise(south, ext)} of {cells} — clamp report reads zero"
    )

    sql_samples = [
        (random.uniform(-180.0, 180.0), random.uniform(-90.0, 90.0)) for _ in range(20_000)
    ]
    sql_samples += [(lon, lat) for lon, lat, _, _ in TEST_VECTORS]
    sql_samples += [(lon, lat) for _, lon, lat, _, _, _ in TILE_VECTORS]
    for frame in ("unit", "metres"):
        worst = check_sql_agrees(sql_samples, frame)
        scale = 1.0 if frame == "unit" else 2.0 * WORLD_HALF_M
        print(
            f"duckdb '{frame}'{'  ' if frame == 'unit' else ''}: "
            f"{len(sql_samples):,} points, worst disagreement {worst:.3e} "
            f"({worst / scale * 65536:.2e} cells)"
        )


def check_sql_agrees(samples: Iterable[tuple[float, float]], frame: Frame = "unit") -> float:
    """The largest disagreement between `sql()` and `project()` over these samples.

    `prepare.py` sends every row through DuckDB and none through Python, so the two have to be the
    same transform rather than two readings of one description. This is what says so.
    """
    import duckdb

    rows = list(samples)
    x_expr, y_expr = sql("lon", "lat", frame)
    con = duckdb.connect()
    con.execute("CREATE TABLE s (i BIGINT, lon DOUBLE, lat DOUBLE)")
    con.executemany(
        "INSERT INTO s VALUES (?, ?, ?)",
        [(i, lon, lat) for i, (lon, lat) in enumerate(rows)],
    )
    got = con.execute(
        f"SELECT i, {x_expr} AS x, {y_expr} AS y FROM s ORDER BY i"
    ).fetchall()
    con.close()

    worst = 0.0
    for i, sx, sy in got:
        px, py = project(rows[i][0], rows[i][1], frame)
        worst = max(worst, abs(sx - px), abs(sy - py))
    return worst


if __name__ == "__main__":
    _self_check()
