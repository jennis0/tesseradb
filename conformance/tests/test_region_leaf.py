"""**The `region` leaf: a drawn shape, or a published one, is the rows inside it — exactly, for
this principal, composed like any other leaf** (`selection-operand.md`; `polygon-membership.md`
§8), black-box, over the real binary, against `oracle.filters`' own definition.

The fixture is `test_shape_membership.py`'s — a lattice of points on tile lines beside a scatter,
two spatial layers whose polygons sit on tile corners, three principals — so the same ties that
test module aims at (a point on an edge, a vertex on a cell corner) are in the lasso's way too.

What is asserted, and by which second reader:

- **A lasso's count is the oracle's, under three principals.** The polygon rides the request as
  the leaf; the sum of the tiles' `matched` is compared with a per-entity even-odd walk over the
  stored (quantised) positions, on-edge inside, masked by the principal's grant — `RegionColumn`
  in `oracle.filters`, which shares no code with the engine's descent. The header says `exact`.
- **A cover is a superset, and the verdict is the shape's alone.** With `max_region_cells` set
  small the same lasso comes back `cover; depth=<d>`: every principal sees the same header, and
  every count is at least the exact one.
- **The leaf by artifact is the artifact's masked count**, and a category filter composes with a
  region as `all_of` — `fx_key` is a rendered number, so the row route and the region's whole-view
  rows meet in one tree.
- **`none_of` over a region is the complement within the visible set** — inside plus outside is
  `visible`, per principal.
- **An unknown id, a suppressed artifact and one withheld by the principal's criterion are one
  response, byte for byte** (C17): the leaf is an empty operand for each, and nothing in the
  status, the headers this test reads or the body's frames — the trailer's clock aside — says which.
- **`region` is a reserved column name** — a declaration naming it is refused at the build.
"""

from __future__ import annotations

import subprocess

import pytest

from oracle.filters import NumericColumn, RegionColumn, evaluate
from oracle.harness import CLI_BIN, REPO_ROOT, build_env, spawn_server, stop_server, write_deployment
from oracle.wire import FRAME_TRAILER, decode_viewport, decode_viewport_artifacts, split_frames

from test_shape_membership import (
    BOXES,
    ID_KEY_HEX,
    PRINCIPALS,
    SHAPES,
    VIEW_ID,
    WHOLE_MAP,
    build_bundle,
    config_toml,
    fixture_points,
    q,
    visible,
)

#: A lasso with a long, jagged edge across the lattice — vertices on and off grid lines.
LASSO = [
    [1500.0, 2000.0],
    [30000.0, 1024.0],
    [52000.0, 20480.0],
    [40000.0, 45000.5],
    [12288.0, 60000.0],
    [4096.0, 33000.0],
    [20480.0, 20480.0],
]
#: A polygon that holds only the lattice column at `y = 1024` — every one an odd source id, so
#: term "2": the principal holding only "1" sees none of its members.
GATED_STRIP = "POLYGON ((0 1024, 65536 1024, 65536 1025, 0 1025, 0 1024))"
GATED = "regions/gated"


def gated_layer_toml() -> str:
    return f"""
[[layer]]
name = "{GATED}"
title = "gated"
views = ["{VIEW_ID}"]
membership = "spatial"
visibility = "public"
artifact_visibility = {{ default = "inherited" }}
require_member_visibility = {{ count = 1 }}
hierarchy = {{ kind = "flat", prune_children = false }}
artifacts = [
  {{ key = "strip", wkt = "{GATED_STRIP}" }}
]

  [layer.shape]
  kind = "polygon"
"""


def region_column(points, served: dict[str, set[int]] | None = None) -> RegionColumn:
    return RegionColumn(
        positions={p[0]: (q(p[1]), q(p[2])) for p in points},
        artifacts=served or {},
        quantise=q,
    )


def matched_sum(server, token: str, filters: dict, **kw) -> tuple[int, str | None]:
    resp = server.viewport_response(token, VIEW_ID, 0, WHOLE_MAP, k=0, filters=filters, **kw)
    tiles, _points = decode_viewport(resp.content)
    return sum(t[2] for t in tiles), resp.headers.get("x-tessera-region")


def visible_sum(server, token: str) -> int:
    resp = server.viewport_response(token, VIEW_ID, 0, WHOLE_MAP, k=0)
    tiles, _points = decode_viewport(resp.content)
    return sum(t[1] for t in tiles)


@pytest.fixture(scope="module")
def region_server(tmp_path_factory):
    points = fixture_points()
    work = tmp_path_factory.mktemp("region-fixture")
    build_bundle(work, points)
    # The gated layer joins the two the shape fixture declares: built beside it, over the same
    # sources, so the withheld-by-criterion case has an artifact to be withheld.
    (work / "gated.toml").write_text(config_toml() + gated_layer_toml())
    bundle = work / "bundle-gated"
    deployment = write_deployment(work / "tessera-gated.toml", bundle=bundle, schema=work / "gated.toml")
    subprocess.run(
        [str(CLI_BIN), "build", "--deployment", str(deployment), "--out", str(bundle), "--mint-external-ids"],
        cwd=REPO_ROOT,
        env=build_env(ID_KEY_HEX),
        check=True,
    )
    server, proc = spawn_server(
        bundle,
        tmp_path_factory.mktemp("region-server"),
        max_k=100_000,
        k_max_marks=100_000,
        theta_target_marks=10**12,
    )
    yield server, points
    stop_server(proc)


@pytest.fixture(scope="module")
def cover_server(tmp_path_factory):
    """The same corpus under a cell budget a lasso cannot fit at the grid."""
    points = fixture_points()
    bundle = build_bundle(tmp_path_factory.mktemp("cover-fixture"), points)
    server, proc = spawn_server(
        bundle,
        tmp_path_factory.mktemp("cover-server"),
        max_k=100_000,
        k_max_marks=100_000,
        theta_target_marks=10**12,
        serve_extra="max_region_cells = 512",
    )
    yield server, points
    stop_server(proc)


def test_a_lasso_counts_the_oracles_inside_exactly_under_three_principals(region_server):
    server, points = region_server
    column = region_column(points)
    leaf = {"region": {"polygon": LASSO}}
    for terms in PRINCIPALS:
        token = server.authorise(terms)["token"]
        candidate = {p[0] for p in visible(points, terms)}
        expected = len(evaluate(leaf, {"region": column}, candidate))
        assert 0 < expected < len(candidate), f"principal {terms}: a real subset ({expected})"
        got, verdict = matched_sum(server, token, leaf)
        assert got == expected, f"principal {terms}: served {got}, oracle {expected}"
        assert verdict == "exact", f"principal {terms}: {verdict!r}"
    # A box, closed on every side, at fractional coordinates.
    box = {"region": {"bbox": [1000.5, 2000.25, 40000.75, 30000.5]}}
    for terms in PRINCIPALS:
        token = server.authorise(terms)["token"]
        candidate = {p[0] for p in visible(points, terms)}
        got, verdict = matched_sum(server, token, box)
        assert got == len(evaluate(box, {"region": column}, candidate))
        assert verdict == "exact"


def test_the_meta_publishes_the_two_bounds_and_no_region_header_without_a_leaf(region_server):
    server, _ = region_server
    token = server.authorise(["1", "2"])["token"]
    selection = server.meta(token)["selection"]
    assert selection["max_region_vertices"] > 0 and selection["max_region_cells"] > 0
    resp = server.viewport_response(token, VIEW_ID, 0, WHOLE_MAP, k=0)
    assert "x-tessera-region" not in resp.headers
    # Over the vertex cap: a 422 naming the count and the cap.
    many = [[float(i), float(i % 7)] for i in range(selection["max_region_vertices"] + 1)]
    refused = server.viewport_request(token, VIEW_ID, 0, WHOLE_MAP, k=0, filters={"region": {"polygon": many}})
    assert refused.status_code == 422, refused.text
    assert str(selection["max_region_vertices"]) in refused.text
    # `wgs84` names the design that would admit it.
    refused = server.viewport_request(
        token, VIEW_ID, 0, WHOLE_MAP, k=0, filters={"region": {"bbox": [0, 0, 1, 1], "space": "wgs84"}}
    )
    assert refused.status_code == 422 and "projections.md" in refused.text


def test_past_the_cell_budget_the_answer_is_a_cover_and_the_verdict_is_the_shapes_alone(cover_server):
    server, points = cover_server
    column = region_column(points)
    leaf = {"region": {"polygon": LASSO}}
    verdicts = set()
    for terms in PRINCIPALS:
        token = server.authorise(terms)["token"]
        candidate = {p[0] for p in visible(points, terms)}
        exact = len(evaluate(leaf, {"region": column}, candidate))
        got, verdict = matched_sum(server, token, leaf)
        assert verdict is not None and verdict.startswith("cover; depth="), verdict
        verdicts.add(verdict)
        assert got >= exact, f"principal {terms}: a cover is a superset ({got} < {exact})"
        assert got < len(candidate), f"principal {terms}: and not the whole map"
    assert len(verdicts) == 1, f"one verdict for every principal, not {verdicts}"


def test_the_leaf_by_artifact_is_its_masked_count_and_composes_with_a_category(region_server):
    server, points = region_server
    for terms in PRINCIPALS:
        token = server.authorise(terms)["token"]
        body = server.viewport(token, VIEW_ID, 0, WHOLE_MAP, k=100_000)
        served = [a for a in decode_viewport_artifacts(body) if a.layer in (SHAPES, BOXES)]
        assert served, f"principal {terms}: the shape layers serve artifacts"
        for artifact in served:
            got, verdict = matched_sum(server, token, {"region": {"artifact": str(artifact.tessera_id)}})
            assert got == artifact.masked_count, f"{artifact.layer}/{artifact.key}: {got} != {artifact.masked_count}"
            assert verdict == "exact"
        # Composed with a numeric leaf: the members whose `fx_key` (the source id) is below a bound.
        artifact = max(served, key=lambda a: a.masked_count)
        bound = 1500
        composed = {"all_of": [{"region": {"artifact": str(artifact.tessera_id)}}, {"fx_key": {"range": {"lt": bound}}}]}
        got, _ = matched_sum(server, token, composed)
        # The oracle: the artifact's members are the points inside its shape, which the shape
        # test's `containing` already defines; here the artifact's own served count bounds it and
        # the bound narrows it.
        column = region_column(points, {str(artifact.tessera_id): _members_of(points, artifact, terms)})
        candidate = {p[0] for p in visible(points, terms)}
        expected = len(evaluate(composed, {"region": column, "fx_key": NumericColumn({p[0]: p[0] for p in points})}, candidate))
        assert got == expected, f"{artifact.layer}/{artifact.key} ∧ fx_key < {bound}: {got} != {expected}"


def _members_of(points, artifact, terms) -> set[int]:
    """The artifact's members this principal sees, from the shape test's own geometry oracle."""
    from test_shape_membership import containing

    return {p[0] for p in visible(points, terms) if artifact.key in containing(p[1], p[2])[artifact.layer]}


def test_none_of_over_a_region_is_the_complement_within_the_visible_set(region_server):
    server, points = region_server
    leaf = {"region": {"polygon": LASSO}}
    for terms in PRINCIPALS:
        token = server.authorise(terms)["token"]
        inside, _ = matched_sum(server, token, leaf)
        outside, verdict = matched_sum(server, token, {"none_of": [leaf]})
        assert verdict == "exact"
        assert inside + outside == visible_sum(server, token), f"principal {terms}: {inside} + {outside}"
        # Inside the lasso but not in a box: `all_of: [region, none_of: [region]]`.
        box = {"region": {"bbox": [1000.5, 2000.25, 40000.75, 30000.5]}}
        both, _ = matched_sum(server, token, {"all_of": [leaf, box]})
        lasso_not_box, _ = matched_sum(server, token, {"all_of": [leaf, {"none_of": [box]}]})
        assert both + lasso_not_box == inside


def test_an_unknown_a_suppressed_and_a_withheld_artifact_are_one_response(region_server):
    server, points = region_server
    # The gated strip: term "2"'s members only, so the principal holding "1" alone is below the
    # criterion and the artifact is withheld from them, while it is served to the others.
    broad = server.authorise(["1", "2"])["token"]
    narrow = server.authorise(["1"])["token"]
    served = [a for a in decode_viewport_artifacts(server.viewport(broad, VIEW_ID, 0, WHOLE_MAP, k=100_000)) if a.layer == GATED]
    assert len(served) == 1 and served[0].masked_count > 0, served
    strip = served[0]
    assert not [a for a in decode_viewport_artifacts(server.viewport(narrow, VIEW_ID, 0, WHOLE_MAP, k=100_000)) if a.layer == GATED]

    def bytes_of(token: str, leaf: dict) -> tuple[list, dict]:
        """The body's frames but the trailer, whose `stream_us` is a clock; and every header but
        the two timing ones (C4's channel, quantified elsewhere and deliberately not asserted)."""
        resp = server.viewport_response(token, VIEW_ID, 0, WHOLE_MAP, k=100_000, filters=leaf)
        headers = {k: v for k, v in resp.headers.items() if k.startswith("x-tessera-") and k not in ("x-tessera-server-us", "x-tessera-admission-us")}
        frames = [(kind, payload) for kind, payload in split_frames(resp.content) if kind != FRAME_TRAILER]
        return frames, headers

    unknown = {"region": {"artifact": "8589934593"}}
    withheld = {"region": {"artifact": str(strip.tessera_id)}}
    body_unknown, head_unknown = bytes_of(narrow, unknown)
    body_withheld, head_withheld = bytes_of(narrow, withheld)
    assert body_withheld == body_unknown and head_withheld == head_unknown
    unknown_count, _ = matched_sum(server, narrow, unknown)
    assert unknown_count == 0

    # Suppressed: served to the broad principal a moment ago, and an empty operand from the ack.
    before, _ = matched_sum(server, broad, withheld)
    assert before == strip.masked_count
    idset = server.meta(broad)["idset"]
    resp = server.changes([{"tessera_id": str(strip.tessera_id), "idset": idset, "op": "suppress"}])
    assert resp.status_code in (200, 202), resp.text
    body_suppressed, head_suppressed = bytes_of(broad, withheld)
    body_unknown_broad, head_unknown_broad = bytes_of(broad, unknown)
    assert body_suppressed == body_unknown_broad and head_suppressed == head_unknown_broad
    resp = server.changes([{"tessera_id": str(strip.tessera_id), "idset": idset, "op": "unsuppress"}])
    assert resp.status_code in (200, 202), resp.text
    after, _ = matched_sum(server, broad, withheld)
    assert after == before


def test_region_is_a_reserved_column_name(tmp_path):
    points = fixture_points()
    work = tmp_path / "reserved"
    work.mkdir()
    # A build whose schema declares an attribute called `region` is refused at the declaration.
    build_bundle(work, points)
    config = work / "shapes.toml"
    config.write_text(
        config_toml().replace('name   = "fx_key"', 'name   = "region"')
    )
    deployment = write_deployment(work / "tessera-reserved.toml", bundle=work / "bundle-reserved", schema=config)
    result = subprocess.run(
        [str(CLI_BIN), "build", "--deployment", str(deployment), "--out", str(work / "bundle-reserved"), "--mint-external-ids"],
        cwd=REPO_ROOT,
        env=build_env(ID_KEY_HEX),
        capture_output=True,
        text=True,
    )
    assert result.returncode != 0
    assert "region" in (result.stderr + result.stdout) and "reserved" in (result.stderr + result.stdout).lower()
