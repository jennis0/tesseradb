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

from oracle.harness import CLI_BIN, REPO_ROOT, build_env, spawn_server, stop_server, write_deployment
from oracle.wire import (
    FRAME_POINTS,
    decode_viewport,
    decode_viewport_artifacts,
    decode_viewport_points,
    split_frames,
)

from test_region_leaf import gated_layer_toml
from test_shape_membership import (
    ID_KEY_HEX,
    PRINCIPALS,
    VIEW_ID,
    WHOLE_MAP,
    build_bundle,
    config_toml,
    fixture_points,
    q,
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
    build_bundle(work, points)
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
        tmp_path_factory.mktemp("highlight-server"),
        max_k=100_000,
        k_max_marks=100_000,
        theta_target_marks=10**12,
    )
    yield server, points
    stop_server(proc)


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
        assert names[0] == "tessera_id" and names[1] == "code"
        assert not any(n.startswith("membership:") for n in names[: names.index("highlighted")]), (
            "the highlight column sits before the membership columns"
        )
        bits = lit.column("highlighted").to_pylist()
        assert len(bits) == lit.num_rows and all(b is not None for b in bits)
        # The oracle: the fixture's own source ids, masked by this principal's grant. The server is
        # uncapped at this budget, so every visible point is served and the two agree exactly.
        want = sum(1 for p in visible(points, terms) if p[0] < 2000)
        assert sum(1 for b in bits if b) == want, f"{terms}: served {sum(bits)}, oracle {want}"


def test_the_artifact_bit_is_the_conjunctions_and_moves_nothing_else(highlight_server):
    server, _points = highlight_server
    for terms in PRINCIPALS:
        token = server.authorise(terms)["token"]
        filters, highlight = below(3000), odd_ids()
        plain = {a.tessera_id: a for a in decode_viewport_artifacts(body(server, token, 0, WHOLE_MAP, filters=filters))}
        assert plain, f"{terms}: the shape layers serve artifacts"
        assert all(a.highlighted is None for a in plain.values()), (
            f"{terms}: no highlight, and yet a bit — a false would answer a question nobody asked"
        )
        lit = {
            a.tessera_id: a
            for a in decode_viewport_artifacts(
                body(server, token, 0, WHOLE_MAP, filters=filters, highlight=highlight)
            )
        }
        conjoined = {
            a.tessera_id: a
            for a in decode_viewport_artifacts(
                body(server, token, 0, WHOLE_MAP, filters={"all_of": [filters, highlight]})
            )
        }
        assert set(lit) == set(plain) == set(conjoined), f"{terms}: the served set moved"
        for tid, artifact in lit.items():
            assert artifact.masked_count == plain[tid].masked_count, f"{terms}: {tid}'s count moved"
            assert artifact.matched == plain[tid].matched, f"{terms}: {tid}'s filter bit moved"
            assert artifact.highlighted == conjoined[tid].matched, (
                f"{terms}: {tid}'s highlight bit is not the conjunction's"
            )


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
            assert projected_table.schema.names == ["tessera_id", "highlighted"], (
                projected_table.schema.names
            )
            assert projected_table.column("tessera_id").to_pylist() == full_table.column("tessera_id").to_pylist()
            assert projected_table.column("highlighted").to_pylist() == full_table.column("highlighted").to_pylist()
            # Nine bytes a point against the render columns' width — the projection is the point.
            full_bytes = sum(len(p) for k, p in split_frames(full) if k == FRAME_POINTS)
            projected_bytes = sum(len(p) for k, p in split_frames(projected) if k == FRAME_POINTS)
            assert projected_bytes < full_bytes, f"{terms} at zoom {zoom}: no bytes saved"
        # No highlight to project to: the projection answers as `"full"` does.
        bare = decode_viewport_points(body(server, token, 2, WHOLE_MAP, point_rows="highlight"))
        plain = decode_viewport_points(body(server, token, 2, WHOLE_MAP))
        assert bare.schema.names == plain.schema.names
        assert bare.column("tessera_id").to_pylist() == plain.column("tessera_id").to_pylist()


def test_an_unknown_point_rows_value_is_refused(highlight_server):
    server, _points = highlight_server
    token = server.authorise(["1", "2"])["token"]
    resp = server.viewport_request(token, VIEW_ID, 0, WHOLE_MAP, k=0, point_rows="bits")
    assert resp.status_code == 422, resp.text


def test_highlighted_is_a_reserved_column_name(tmp_path):
    points = fixture_points()
    work = tmp_path / "reserved"
    work.mkdir()
    build_bundle(work, points)
    config = work / "shapes.toml"
    config.write_text(config_toml().replace('name   = "fx_key"', 'name   = "highlighted"'))
    deployment = write_deployment(work / "tessera-reserved.toml", bundle=work / "bundle-reserved", schema=config)
    result = subprocess.run(
        [str(CLI_BIN), "build", "--deployment", str(deployment), "--out", str(work / "bundle-reserved"), "--mint-external-ids"],
        cwd=REPO_ROOT,
        env=build_env(ID_KEY_HEX),
        capture_output=True,
        text=True,
    )
    assert result.returncode != 0
    assert "highlighted" in (result.stderr + result.stdout)
    assert "reserved" in (result.stderr + result.stdout).lower()


#: A clustering and the labels attached to it — registered at runtime through the control plane,
#: because no built fixture in this suite declares a dependent layer and the rule under test is
#: about one layer hanging from another (decision 0089).
#:
#: **Named without a slash**, unlike the path-shaped layers a corpus declares:
#: `PUT /control/layers/{name}/artifacts` matches one path segment, so a name carrying one is a
#: 404 at the router rather than a refusal from the handler. That is a fact about the control
#: plane's addressing and not about this case, so it is worked around here and reported.
CLUSTERS = "case-clusters"
LABELS = "case-labels"


def _layer(name: str, *, depends_on: list[str], supplied: bool) -> dict:
    layer = {
        "name": name,
        "title": name,
        "views": [VIEW_ID],
        "membership": "enumerated",
        "visibility": None,
        "artifact_visibility": {"field": None, "default": "inherited"},
        "require_member_visibility": None,
        "hierarchy": {"kind": "flat", "prune_children": False},
        "content": {
            "computed": [],
            # `inherited`: the content is true whether or not a document exists, so containment is
            # vacuous and no generating set travels with it (C28).
            "supplied": (
                [{"name": "label", "type": "text", "require_member_visibility": "inherited"}]
                if supplied
                else []
            ),
        },
        "depends_on": depends_on,
        "levels": [],
        "layout": None,
        "shape": None,
    }
    return layer


def test_a_dependent_artifact_carries_its_targets_highlight_bit(highlight_server):
    """**A label's `highlighted` is its cluster's**, exactly as its `matched` is (contracts §3.2
    r74; decision 0104's argument) — and its `masked_count` is its own.

    The case is a label whose own membership would answer differently: the cluster holds the points
    whose `fx_key` is below the bound and the label holds ten that are not, so a bit computed over
    the label's own membership reads `false` beside a cluster reading `true`. **The same clause is
    sent in `filters` and in `highlight`**, so the two answers about one cluster are comparable: a
    label that inherited one bit and kept its own for the other disagrees with itself here, and no
    assertion about `matched` alone can see that — the label's own answer is a well-formed `false`.
    """
    server, _points = highlight_server
    token = server.authorise(["1", "2"])["token"]
    idset = server.meta(token)["idset"]
    table = decode_viewport_points(body(server, token, 0, WHOLE_MAP))
    ids = table.column("tessera_id").to_pylist()
    keys = table.column("fx_key").to_pylist()
    inside = [str(i) for i, k in zip(ids, keys) if k < 2000]
    outside = [str(i) for i, k in zip(ids, keys) if k >= 2000]
    assert len(inside) > 10 and len(outside) > 10, "the fixture must plant both cases"

    for name, depends_on, supplied in ((CLUSTERS, [], False), (LABELS, [CLUSTERS], True)):
        resp = server.register_layer(_layer(name, depends_on=depends_on, supplied=supplied))
        assert resp.status_code == 201, resp.text
    resp = server.publish_artifacts(
        CLUSTERS,
        addressing="tessera",
        idset=idset,
        artifacts=[{"key": "c-hit", "members": inside}],
    )
    assert resp.status_code in (200, 201, 202), resp.text
    resp = server.publish_artifacts(
        LABELS,
        addressing="tessera",
        idset=idset,
        artifacts=[
            {
                "key": "label-hit",
                "members": outside[:10],
                "content": [{"values": ["hit"]}],
                "attached_to": {"layer": CLUSTERS, "level": 0, "key": "c-hit"},
            }
        ],
    )
    assert resp.status_code in (200, 201, 202), resp.text

    clause = below(2000)
    served = {
        (a.layer, a.key): a
        for a in decode_viewport_artifacts(
            body(server, token, 0, WHOLE_MAP, layers=[CLUSTERS, LABELS], filters=clause, highlight=clause)
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
