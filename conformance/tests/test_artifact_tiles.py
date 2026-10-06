"""**`POST /v1/artifacts/viewport`, tile by tile**, against an oracle that reads nothing the server
says about where an artifact is or how many members it has.

The oracle places each of its own points in a tile from the point's source position, takes the
viewer's visible points from the fixture's own access rule, and asks the fixture's own geometry
which artifacts of the two spatial layers hold each point, and its own residue rule which artifacts
of an enumerated layer of overlapping groups hold it. That layer declares no layout, so the server
chooses how to store it. For every tile it then names the
artifacts with a visible member there, ordered by whole visible count and then `tessera_id`, the
first `per_tile` of them. The identifiers are learnt from the served rows by key, never from their
order. Each tile's frame is compared with that, frame by frame, under a quota small enough to bind,
for a viewer who sees part of the corpus and one who sees all of it, before and after a deny.
"""

from __future__ import annotations

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from oracle import morton
from oracle.harness import JOIN_COLUMN, cli_build, spawn_server, stop_server, write_deployment
from oracle.wire import decode_artifact_frames, decode_viewport_artifacts, decode_viewport_points

from test_region_leaf import gated_layer_toml
from test_shape_membership import (
    BOXES,
    SHAPES,
    VIEW_ID,
    WHOLE_MAP,
    build_bundle,
    config_toml,
    containing,
    fixture_points,
    q,
    visible,
)

GROUPS = "groups"
LAYERS = [SHAPES, BOXES, GROUPS]
PER_TILE = 2


def groups_of(fx: int) -> set[str]:
    """The enumerated layer's artifacts holding a point: two overlapping residue classes."""
    return {f"g{fx % 5}", f"h{fx % 3}"}


def holding(fx: int, x: float, y: float) -> dict[str, set[str]]:
    """Every layer's artifacts holding a point."""
    return containing(x, y) | {GROUPS: groups_of(fx)}


def groups_layer_toml() -> str:
    return f"""
[[layer]]
name = "{GROUPS}"
title = "groups"
views = ["{VIEW_ID}"]
membership = "enumerated"
value_set = "open"
visibility = "public"
artifact_visibility = {{ default = "inherited" }}
require_member_visibility = "none"
hierarchy = {{ kind = "flat", prune_children = false }}

  [layer.members]
  source = "groups"
"""


@pytest.fixture(scope="module")
def tiles_server(tmp_path_factory):
    points = fixture_points()
    work = tmp_path_factory.mktemp("artifact-tiles-fixture")
    build_bundle(work, points)
    rows = [(key, fx) for fx, _x, _y in points for key in sorted(groups_of(fx))]
    pq.write_table(
        pa.table(
            {
                "key": pa.array([key for key, _ in rows], pa.string()),
                JOIN_COLUMN: pa.array([fx for _, fx in rows], pa.uint64()),
            }
        ),
        work / "groups.parquet",
    )
    config = config_toml().replace('pairs  = "pairs.parquet"', 'pairs  = "pairs.parquet"\ngroups = "groups.parquet"', 1)
    assert "groups.parquet" in config
    (work / "gated.toml").write_text(config + gated_layer_toml() + groups_layer_toml())
    bundle = work / "bundle-gated"
    deployment = write_deployment(work / "tessera-gated.toml", bundle=bundle, schema=work / "gated.toml")
    cli_build(deployment, bundle)
    server, proc = spawn_server(
        bundle,
        tmp_path_factory.mktemp("artifact-tiles-server"),
        max_k=100_000,
        k_max_marks=100_000,
        theta_target_marks=10**12,
    )
    yield server, points
    stop_server(proc)


def tile_of(x: float, y: float, zoom: int) -> int:
    """The depth-`zoom` tile holding a point, from its source position."""
    cell_code, _residual = morton.split32(q(x), q(y))
    return cell_code >> (32 - 2 * zoom)


def oracle(points, ids: dict[tuple[str, str], int], zoom: int) -> dict[int, list[tuple[str, str, int]]]:
    """Every tile holding a visible point: the first `PER_TILE` artifacts of each layer with a
    visible member there, by whole visible count and then `tessera_id`, as `(layer, key, count)`."""
    whole: dict[tuple[str, str], int] = {}
    present: dict[int, set[tuple[str, str]]] = {}
    for fx, x, y in points:
        tile = present.setdefault(tile_of(x, y, zoom), set())
        for layer, keys in holding(fx, x, y).items():
            for key in keys:
                whole[(layer, key)] = whole.get((layer, key), 0) + 1
                tile.add((layer, key))
    out = {}
    for tile, here in present.items():
        rows = []
        for layer in LAYERS:
            ranked = sorted(
                (key for lay, key in here if lay == layer),
                key=lambda key: (-whole[(layer, key)], ids[(layer, key)]),
            )
            rows.extend((layer, key, whole[(layer, key)]) for key in ranked[:PER_TILE])
        out[tile] = rows
    return out


def assert_tiles_match(server, token: str, seen, zoom: int, at: str) -> dict[int, list]:
    """Ask every tile at `zoom`, one frame each in order, and compare each with the oracle's."""
    tiles = list(range(4**zoom))
    body = server.artifacts_viewport(token, VIEW_ID, zoom, None, tiles=tiles, layers=LAYERS, per_tile=PER_TILE)
    frames, _trailer = decode_artifact_frames(body)
    assert len(frames) == len(tiles), f"{at}: one frame per tile asked"
    whole = server.artifacts_viewport(token, VIEW_ID, zoom, WHOLE_MAP, layers=LAYERS)
    ids = {(a.layer, a.key): a.tessera_id for a in decode_viewport_artifacts(whole)}
    want = oracle(seen, ids, zoom)
    got = {}
    for tile, (named, rows) in zip(tiles, frames):
        assert named in (tile, None), f"{at}: frame {tile} names tile {named}"
        got[tile] = [(a.layer, a.key, a.masked_count) for a in rows]
        assert got[tile] == want.get(tile, []), f"{at}: tile {tile}"
    return got


def test_each_tile_is_the_oracles_under_a_quota_for_a_partial_and_a_whole_viewer(tiles_server):
    server, points = tiles_server
    for terms in (["1"], ["1", "2"]):
        token = server.authorise(terms)["token"]
        seen = visible(points, terms)
        for zoom in (1, 2, 3):
            got = assert_tiles_match(server, token, seen, zoom, f"{terms} at zoom {zoom}")
            assert any(len([r for r in rows if r[0] == SHAPES]) == PER_TILE for rows in got.values()), (
                f"{terms} at zoom {zoom}: the quota binds somewhere"
            )
            spans = {}
            for tile, rows in got.items():
                for layer, key, _count in rows:
                    spans.setdefault((layer, key), set()).add(tile)
            assert any(len(t) > 1 for t in spans.values()), f"{terms} at zoom {zoom}: an artifact spans tiles"


def test_a_denied_member_leaves_its_tile_and_every_tile_stays_the_oracles(tiles_server):
    server, points = tiles_server
    terms = ["1", "2"]
    token = server.authorise(terms)["token"]
    zoom = 2
    seen = visible(points, terms)
    got = assert_tiles_match(server, token, seen, zoom, "before the deny")
    # An artifact served in two tiles or more, and one of them.
    tiles_of: dict[tuple[str, str], list[int]] = {}
    for tile, rows in got.items():
        for layer, key, _count in rows:
            tiles_of.setdefault((layer, key), []).append(tile)
    (layer, key), tiles = next((k, t) for k, t in sorted(tiles_of.items()) if len(t) > 1)
    tile = tiles[0]
    denied = {fx for fx, x, y in seen if tile_of(x, y, zoom) == tile and key in holding(fx, x, y)[layer]}
    table = decode_viewport_points(server.viewport(token, VIEW_ID, 0, WHOLE_MAP, k=100_000))
    by_fx = dict(zip(table.column("fx_key").to_pylist(), table.column("tessera_id").to_pylist()))
    for fx in denied:
        assert server.change(by_fx[fx], "suppress").status_code == 200
    try:
        after = assert_tiles_match(
            server, token, [p for p in seen if p[0] not in denied], zoom, "after the deny"
        )
        assert (layer, key) not in {(row[0], row[1]) for row in after[tile]}, f"{key} is still in tile {tile}"
    finally:
        for fx in denied:
            assert server.change(by_fx[fx], "unsuppress").status_code == 200
