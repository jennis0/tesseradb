"""**The `highlight` field: a second expression over the same candidate, three answers, and a draw
that does not move** (`highlight-and-hierarchy.md` §2), black-box, over the real binary.

The fixture is `test_shape_membership.py`'s, with `test_region_leaf.py`'s gated strip beside it, so
the artifacts frame has rows to carry a bit and three principals see three different corpora.

What is asserted, and by which second reader:

- **The served set is identical with and without a highlight.** The tiles frame's
  `(tile, visible, matched, served)` and the whole points stream are compared byte for byte
  between a request carrying a highlight and the same request without one. This is the property
  that makes it a highlight rather than a filter, and the one whose failure looks like the feature
  working.
- **`highlighted = matched` with no highlight, `highlighted ≤ matched ≤ visible` with one**, and
  the per-tile count is exactly the `matched` a request carrying `all_of[filters, highlight]` in
  `filters` reports — C32's argument, checked.
- **The per-point bit is present exactly when the request carried a highlight**, and its total is
  the oracle's over the fixture's own values.
- **The artifacts frame's bit is decision 0104's under the conjunction**, `null` without a
  highlight, and it moves neither the served set nor any masked count.
- **A dependent artifact inherits the two filter bits and not the count.** The count copy was
  withdrawn by the owner's ruling of 2026-09-18; a label's row carries the count of the membership
  it is served over. ⊘ The `target` column that replaced the client's join by count is not
  asserted here: `reference/oracle/wire.py` does not decode it yet, and that file is outside this
  track.
- **`point_rows: "highlight"` serves the same rows in the same split** in a two-column frame, and
  answers as `"full"` does when the request carried no highlight.
- **`highlighted` is a reserved column name** — a declaration naming it is refused at the build.
"""

from __future__ import annotations

import subprocess

import pytest

from oracle.harness import (
    CLI_BIN,
    JOIN_FIELD,
    REPO_ROOT,
    spawn_server,
    stop_server,
    write_deployment,
)
from oracle.wire import (
    FRAME_POINTS,
    decode_viewport,
    decode_viewport_artifacts,
    decode_viewport_points,
    split_frames,
)

from test_region_leaf import gated_layer_toml
from test_shape_membership import (
    PRINCIPALS,
    VIEW_ID,
    WHOLE_MAP,
    build_bundle,
    build_with_layers,
    config_toml,
    fixture_points,
    visible,
)

#: The viewports the sweeps run over. The whole map at three depths, plus two boxes that are
#: genuinely different slices of row space rather than three spellings of one.
CELLS = [
    (0, WHOLE_MAP),
    (2, WHOLE_MAP),
    (3, WHOLE_MAP),
    (3, [0.0, 0.0, 30000.0, 30000.0]),
    (4, [10000.0, 10000.0, 50000.0, 50000.0]),
]

#: `fx_key` is the source id, rendered and indexed by the shape fixture's own schema, so a range
#: over it is a clause whose answer this module can compute directly.
def below(bound: int) -> dict:
    return {"fx_key": {"range": {"lt": bound}}}


def odd_ids() -> dict:
    """A clause selecting the lattice's odd source ids — a real subset that shares no factor with
    the term model, so masking and highlighting genuinely cut across each other."""
    return {"fx_key": {"range": {"gte": 1}}}


def body(server, token: str, zoom: int, bbox, **kw):
    resp = server.viewport_request(token, VIEW_ID, zoom, bbox, k=100_000, **kw)
    assert resp.status_code == 200, resp.text
    return resp.content


def artifacts_body(server, token: str, zoom: int, bbox, **kw):
    """The `/v1/artifacts/viewport` body of the same request."""
    resp = server.artifacts_viewport_request(token, VIEW_ID, zoom, bbox, **kw)
    assert resp.status_code == 200, resp.text
    return resp.content


def tiles_of(content: bytes):
    tiles, _points = decode_viewport(content)
    return tiles


def points_of(content: bytes):
    _tiles, points = decode_viewport(content)
    return points


@pytest.fixture(scope="module")
def highlight_server(tmp_path_factory):
    points = fixture_points()
    work = tmp_path_factory.mktemp("highlight-fixture")
    bundle = build_with_layers(work, "gated", gated_layer_toml())
    server, proc = spawn_server(
        bundle,
        tmp_path_factory.mktemp("highlight-server"),
        max_k=100_000,
        k_max_marks=100_000,
        theta_target_marks=10**12,
    )
    yield server, points
    stop_server(proc)


@pytest.mark.writes_nothing
def test_the_served_set_is_identical_with_and_without_a_highlight(highlight_server):
    server, _points = highlight_server
    for terms in PRINCIPALS:
        token = server.authorise(terms)["token"]
        for zoom, bbox in CELLS:
            for filters in (None, below(3000)):
                kw = {} if filters is None else {"filters": filters}
                plain = body(server, token, zoom, bbox, **kw)
                for highlight in (below(1000), odd_ids(), {"fx_key": {"range": {"gte": 10**9}}}):
                    lit = body(server, token, zoom, bbox, highlight=highlight, **kw)
                    what = f"{terms} at zoom {zoom} over {bbox}, filters={filters is not None}"
                    assert [t[:4] for t in tiles_of(lit)] == [t[:4] for t in tiles_of(plain)], what
                    # The whole points stream, byte for byte: the highlight adds a column, so the
                    # frames differ, but the identities and their order may not.
                    assert points_of(lit) == points_of(plain), what


@pytest.mark.writes_nothing
def test_the_counts_nest_and_the_highlighted_count_is_the_conjunctions(highlight_server):
    server, _points = highlight_server
    for terms in PRINCIPALS:
        token = server.authorise(terms)["token"]
        for zoom, bbox in CELLS:
            for tile in tiles_of(body(server, token, zoom, bbox)):
                assert tile[4] == tile[2], f"{terms}: an absent highlight is the identity"
            filters, highlight = below(3000), odd_ids()
            lit = tiles_of(body(server, token, zoom, bbox, filters=filters, highlight=highlight))
            for tile, visible_n, matched, _served, highlighted in lit:
                assert highlighted <= matched <= visible_n, f"{terms}: tile {tile}"
            conjoined = tiles_of(
                body(server, token, zoom, bbox, filters={"all_of": [filters, highlight]})
            )
            assert [(t[0], t[4]) for t in lit] == [(t[0], t[2]) for t in conjoined], (
                f"{terms} at zoom {zoom}: the highlight's count is not the conjunction's"
            )


@pytest.mark.writes_nothing
def test_the_point_bit_is_present_only_with_a_highlight_and_is_the_oracles(highlight_server):
    server, points = highlight_server
    for terms in PRINCIPALS:
        token = server.authorise(terms)["token"]
        plain = decode_viewport_points(body(server, token, 0, WHOLE_MAP))
        assert "highlighted" not in plain.schema.names, "no highlight, no column"
        highlight = below(2000)
        lit = decode_viewport_points(body(server, token, 0, WHOLE_MAP, highlight=highlight))
        assert "highlighted" in lit.schema.names
        # Position is contract: after the render scalars and before any membership column.
        names = lit.schema.names
        assert names[0] == "mosaica_id" and names[1] == "code"
        assert not any(n.startswith("membership:") for n in names[: names.index("highlighted")]), (
            "the highlight column sits before the membership columns"
        )
        bits = lit.column("highlighted").to_pylist()
        assert len(bits) == lit.num_rows and all(b is not None for b in bits)
        # The oracle: the fixture's own source ids, masked by this principal's grant. The server is
        # uncapped at this budget, so every visible point is served and the two agree exactly.
        want = sum(1 for p in visible(points, terms) if p[0] < 2000)
        assert sum(1 for b in bits if b) == want, f"{terms}: served {sum(bits)}, oracle {want}"


@pytest.mark.writes_nothing
def test_the_artifact_bit_is_the_conjunctions_and_moves_nothing_else(highlight_server):
    server, _points = highlight_server
    for terms in PRINCIPALS:
        token = server.authorise(terms)["token"]
        filters, highlight = below(3000), odd_ids()
        plain = {a.mosaica_id: a for a in decode_viewport_artifacts(artifacts_body(server, token, 0, WHOLE_MAP, filters=filters))}
        assert plain, f"{terms}: the shape layers serve artifacts"
        assert all(a.highlighted is None for a in plain.values()), (
            f"{terms}: no highlight, and yet a bit — a false would answer a question nobody asked"
        )
        lit = {
            a.mosaica_id: a
            for a in decode_viewport_artifacts(
                artifacts_body(server, token, 0, WHOLE_MAP, filters=filters, highlight=highlight)
            )
        }
        conjoined = {
            a.mosaica_id: a
            for a in decode_viewport_artifacts(
                artifacts_body(server, token, 0, WHOLE_MAP, filters={"all_of": [filters, highlight]})
            )
        }
        assert set(lit) == set(plain) == set(conjoined), f"{terms}: the served set moved"
        for tid, artifact in lit.items():
            assert artifact.masked_count == plain[tid].masked_count, f"{terms}: {tid}'s count moved"
            assert artifact.matched == plain[tid].matched, f"{terms}: {tid}'s filter bit moved"
            assert artifact.highlighted == conjoined[tid].matched, (
                f"{terms}: {tid}'s highlight bit is not the conjunction's"
            )


@pytest.mark.writes_nothing
def test_the_highlight_projection_serves_the_same_rows_in_a_two_column_frame(highlight_server):
    server, _points = highlight_server
    for terms in PRINCIPALS:
        token = server.authorise(terms)["token"]
        for zoom, bbox in CELLS:
            kw = {"filters": below(3000), "highlight": odd_ids()}
            full = body(server, token, zoom, bbox, **kw)
            projected = body(server, token, zoom, bbox, point_rows="highlight", **kw)
            assert [t[:4] for t in tiles_of(projected)] == [t[:4] for t in tiles_of(full)]
            full_table = decode_viewport_points(full)
            projected_table = decode_viewport_points(projected)
            assert projected_table.schema.names == ["mosaica_id", "highlighted"], (
                projected_table.schema.names
            )
            assert projected_table.column("mosaica_id").to_pylist() == full_table.column("mosaica_id").to_pylist()
            assert projected_table.column("highlighted").to_pylist() == full_table.column("highlighted").to_pylist()
            # Nine bytes a point against the render columns' width — the projection is the point.
            full_bytes = sum(len(p) for k, p in split_frames(full) if k == FRAME_POINTS)
            projected_bytes = sum(len(p) for k, p in split_frames(projected) if k == FRAME_POINTS)
            assert projected_bytes < full_bytes, f"{terms} at zoom {zoom}: no bytes saved"
        # No highlight to project to: the projection answers as `"full"` does.
        bare = decode_viewport_points(body(server, token, 2, WHOLE_MAP, point_rows="highlight"))
        plain = decode_viewport_points(body(server, token, 2, WHOLE_MAP))
        assert bare.schema.names == plain.schema.names
        assert bare.column("mosaica_id").to_pylist() == plain.column("mosaica_id").to_pylist()


@pytest.mark.writes_nothing
def test_an_unknown_point_rows_value_is_refused(highlight_server):
    server, _points = highlight_server
    token = server.authorise(["1", "2"])["token"]
    resp = server.viewport_request(token, VIEW_ID, 0, WHOLE_MAP, k=0, point_rows="bits")
    assert resp.status_code == 422, resp.text


@pytest.mark.writes_nothing
def test_highlighted_is_a_reserved_column_name(tmp_path):
    points = fixture_points()
    work = tmp_path / "reserved"
    work.mkdir()
    build_bundle(work, points)
    config = work / "shapes.toml"
    config.write_text(config_toml().replace('name   = "fx_key"', 'name   = "highlighted"'))
    deployment = write_deployment(work / "mosaica-reserved.toml", bundle=work / "bundle-reserved", schema=config)
    result = subprocess.run(
        [str(CLI_BIN), "build", "--deployment", str(deployment), "--out", str(work / "bundle-reserved")],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
    )
    assert result.returncode != 0
    assert "highlighted" in (result.stderr + result.stdout)
    assert "reserved" in (result.stderr + result.stdout).lower()


#: A clustering and a label attached to it, declared at the build: the rule under test is about
#: one layer hanging from another (decision 0089). The cluster holds the points whose `fx_key`, the
#: source id, is below [`BOUND`]; the label holds ten that are not.
CLUSTERS = "case-clusters"
LABELS = "case-labels"
BOUND = 2000


def dependent_layers_toml() -> str:
    cluster = ", ".join(str(sid) for sid in range(BOUND))
    label = ", ".join(str(sid) for sid in range(BOUND, BOUND + 10))
    attached = f'attached_layer = "{CLUSTERS}", attached_key = "c-hit"'
    contents = 'contents = [["hit"]]'
    return f"""
[[layer]]
name = "{CLUSTERS}"
title = "{CLUSTERS}"
views = ["{VIEW_ID}"]
membership = "enumerated"
visibility = "public"
artifact_visibility = {{ default = "inherited" }}
require_member_visibility = "none"
hierarchy = {{ kind = "flat", prune_children = false }}
artifacts = [
  {{ key = "c-hit", members = {{ {JOIN_FIELD} = [{cluster}] }} }},
]

[[layer]]
name = "{LABELS}"
title = "{LABELS}"
views = ["{VIEW_ID}"]
membership = "enumerated"
visibility = "public"
artifact_visibility = {{ default = "inherited" }}
require_member_visibility = "none"
hierarchy = {{ kind = "flat", prune_children = false }}
depends_on = ["{CLUSTERS}"]
artifacts = [
  {{ key = "label-hit", members = {{ {JOIN_FIELD} = [{label}] }}, {attached}, {contents} }},
]

  [[layer.content.supplied]]
  name = "label"
  type = "text"
  require_member_visibility = "inherited"
"""


@pytest.fixture(scope="module")
def dependent_server(tmp_path_factory):
    """The highlight fixture with [`CLUSTERS`] and [`LABELS`] beside its layers."""
    work = tmp_path_factory.mktemp("highlight-dependent")
    bundle = build_with_layers(work, "dependent", gated_layer_toml() + dependent_layers_toml())
    server, proc = spawn_server(
        bundle,
        tmp_path_factory.mktemp("highlight-dependent-server"),
        max_k=100_000,
        k_max_marks=100_000,
        theta_target_marks=10**12,
    )
    yield server
    stop_server(proc)


@pytest.mark.writes_nothing
def test_a_dependent_artifact_carries_its_targets_highlight_bit(dependent_server):
    """**A label's `highlighted` is its cluster's**, exactly as its `matched` is (contracts §3.2
    r74; decision 0104's argument) — and its `masked_count` is its own.

    The case is a label whose own membership would answer differently: the cluster holds the points
    whose `fx_key` is below the bound and the label holds ten that are not, so a bit computed over
    the label's own membership reads `false` beside a cluster reading `true`. **The same clause is
    sent in `filters` and in `highlight`**, so the two answers about one cluster are comparable: a
    label that inherited one bit and kept its own for the other disagrees with itself here, and no
    assertion about `matched` alone can see that — the label's own answer is a well-formed `false`.
    """
    server = dependent_server
    token = server.authorise(["1", "2"])["token"]
    keys = decode_viewport_points(body(server, token, 0, WHOLE_MAP)).column("fx_key").to_pylist()
    assert sorted(k for k in keys if k < BOUND + 10) == list(range(BOUND + 10)), (
        "the principal must see every member of both artifacts"
    )

    clause = below(BOUND)
    served = {
        (a.layer, a.key): a
        for a in decode_viewport_artifacts(
            artifacts_body(server, token, 0, WHOLE_MAP, layers=[CLUSTERS, LABELS], filters=clause, highlight=clause)
        )
        if a.layer in (CLUSTERS, LABELS)
    }
    cluster = served[(CLUSTERS, "c-hit")]
    label = served[(LABELS, "label-hit")]
    assert cluster.matched is True and cluster.highlighted is True, cluster
    assert label.matched == cluster.matched, "the label answers for its cluster (decision 0104)"
    assert label.highlighted == cluster.highlighted, (
        "and in the second field exactly as in the first — a label whose own members carry none "
        "of the value must still answer for its cluster"
    )
    # **And the count is the label's own** (owner ruling, 2026-09-18, which withdrew the copy).
    # This label declares ten members of its own, so by decision 0145 it is served over its own
    # generating set rather than borrowing its target's — and those ten are exactly the points the
    # cluster does *not* hold, so the two counts are different numbers and a copy would show here.
    assert label.masked_count == 10, label
    assert label.masked_count != cluster.masked_count, (
        "a label's row carries the count of the membership it is served over, never its cluster's"
    )
