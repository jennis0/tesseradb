"""The WGS84 to view-frame transform, as a second implementation of the one the engine runs.

**This module was written to be replaced, and it has been.** It placed both built geographic
corpora while Tessera had no projection layer; it places none now. `tessera_spatial::projection`
is the transform, a view declares which one it uses, and every `prepare.py` in this tree emits
`lon`/`lat` in degrees for the build to transform (`docs/design/projections.md` §3).

What it is instead is the **independent check on the engine's arithmetic**, in both directions.
The vectors are data in `projection-vectors.json` beside this file — one description that two
languages read, rather than two implementations of one — and `python -m
test_corpora.common.projection` checks this half while `tessera_spatial::projection` checks the
other. Beyond the vectors, `tessera_spatial::projection`'s `agrees_with_the_python_reference` runs
this module over 100,000 sampled coordinates and requires the same *stored* position rather than a
similar float. A built corpus is checked the same way: on 2026-08-30 every point of both geographic
rungs had its expected 32-bit fixed-point position recomputed here from the source degrees and
compared against what the bundle holds, and all 87,094,949 of them agreed — recorded in each rung's
own README.

Keeping a second implementation is the point. A single implementation checked against its own
output cannot fail, and the quantised comparison admits no tolerance: two transforms that place a
point in different cells have disagreed about where it is, and nothing downstream can see that
they did.

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

#: The whole domain, as the WGS84 box a declaration writes for `extent` (`projections.md` §2, §4.2).
#: Both geographic rungs declare exactly this box, and it snaps to the world square at offset 0.
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


def is_clipped(lat: float) -> bool:
    """Whether this latitude falls outside the projected domain and is being moved onto its edge.

    The build's clamp report structurally cannot see these: clipping lands them at *exactly* the
    frame maximum, and `contracts` §2.5 says a point at the maximum is not clamped, which is why
    the build counts clipping separately (`projections.md` §7). This is the same predicate, over
    the source, for a survey taken before any build has run.
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


if __name__ == "__main__":
    _self_check()
