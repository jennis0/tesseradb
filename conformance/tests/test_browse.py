"""**`POST /v1/artifacts/browse`: a layer's hierarchy by lineage, independent of the viewport**
(`highlight-and-hierarchy.md` §4), black-box, over the real binary.

The fixture is `test_shape_membership.py`'s two spatial layers with `test_region_leaf.py`'s gated
strip beside them, so three principals see three different populations and one artifact is withheld
from one of them by its own criterion.

What is asserted, and by which second reader:

- **Every row is the artifacts frame's own row.** The page's `tessera_id`, `key` and `masked_count`
  are compared against what a zoom-0 viewport serves the same principal — the artifacts frame being
  the reader that already exists for those numbers — so a browse that computed its counts a second
  way is caught rather than ratified.
- **The gate runs before the page.** The artifact withheld from the narrow principal is absent from
  every form at once, and their page's `next` counts only rows they may see.
- **The order is total and the cursor walks it exactly**: paging one row at a time reproduces the
  unpaged answer.
- **The counts under a filter**: `matched_count` is the oracle's `|membership ∩ M_auth ∩ filter|`,
  existence and `masked_count` do not move, and a row matching nothing is still served.
- **The refusals name deployment schema and never an artifact**: an unknown layer, a `level` on a
  one-level kind, `limit = 0` and both `parent` and `q` refuse; an unknown, foreign or withheld
  `parent` answers an empty page.
- **`/v1/meta` publishes `selection.max_browse_rows`**, and `limit` clamps to it.
"""

from __future__ import annotations

import subprocess

import pytest

from oracle.harness import CLI_BIN, REPO_ROOT, build_env, spawn_server, stop_server, write_deployment
from oracle.wire import decode_viewport_artifacts

from test_region_leaf import GATED, gated_layer_toml
from test_shape_membership import (
    BOXES,
    ID_KEY_HEX,
    PRINCIPALS,
    SHAPES,
    VIEW_ID,
    WHOLE_MAP,
    build_bundle,
    config_toml,
    containing,
    fixture_points,
    visible,
)


@pytest.fixture(scope="module")
def browse_server(tmp_path_factory):
    points = fixture_points()
    work = tmp_path_factory.mktemp("browse-fixture")
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
        tmp_path_factory.mktemp("browse-server"),
        max_k=100_000,
        k_max_marks=100_000,
        theta_target_marks=10**12,
    )
    yield server, points
    stop_server(proc)


def page(server, token: str, **body) -> dict:
    resp = server.browse(token, view=VIEW_ID, **body)
    assert resp.status_code == 200, resp.text
    return resp.json()


def served_artifacts(server, token: str, layer: str) -> dict:
    """What the viewport's own artifacts frame serves this principal for `layer` — the second
    reader for every identifier and every count browse reports."""
    return {
        str(a.tessera_id): a
        for a in decode_viewport_artifacts(server.viewport(token, VIEW_ID, 0, WHOLE_MAP, k=100_000))
        if a.layer == layer
    }


def test_a_page_is_the_artifacts_frames_own_rows(browse_server):
    server, _points = browse_server
    for terms in PRINCIPALS:
        token = server.authorise(terms)["token"]
        for layer in (SHAPES, BOXES):
            frame = served_artifacts(server, token, layer)
            assert frame, f"principal {terms}: {layer} serves artifacts"
            out = page(server, token, layer=layer, limit=200)
            rows = {row["tessera_id"]: row for row in out["artifacts"]}
            assert set(rows) == set(frame), (
                f"principal {terms}/{layer}: browse and the viewport serve different artifacts"
            )
            for tid, row in rows.items():
                assert row["masked_count"] == frame[tid].masked_count, f"{layer}/{tid}"
                assert row.get("key") == frame[tid].key
                assert "matched_count" not in row, "no filter, no question"
                assert row["parent_ids"] == [], "a flat layer names no parent"
            assert out["parents"] == [], "`parents` is the children form's"
            # The total order, as a property: count descending, then identifier ascending.
            keyed = [(-row["masked_count"], int(row["tessera_id"])) for row in out["artifacts"]]
            assert keyed == sorted(keyed), f"principal {terms}/{layer}: the order is not total"


def test_the_gate_runs_before_the_page(browse_server):
    server, _points = browse_server
    broad = server.authorise(["1", "2"])["token"]
    narrow = server.authorise(["1"])["token"]
    # The gated strip holds only term "2"'s members, so it clears its criterion for one principal
    # and not the other — the same split `test_region_leaf.py` uses.
    broad_rows = page(server, broad, layer=GATED, limit=200)["artifacts"]
    assert len(broad_rows) == 1 and broad_rows[0]["masked_count"] > 0, broad_rows
    narrow_rows = page(server, narrow, layer=GATED, limit=200)
    assert narrow_rows["artifacts"] == [] and "next" not in narrow_rows, (
        "the withheld artifact is absent from the page and the page is complete — not short"
    )
    # And it is absent from the search form too, which is the same gate reached another way.
    strip = broad_rows[0]
    assert page(server, broad, layer=GATED, q="strip")["artifacts"] == broad_rows
    assert page(server, narrow, layer=GATED, q="strip")["artifacts"] == []
    # Its identifier as a `parent` answers an empty page rather than refusing.
    withheld = page(server, narrow, layer=GATED, parent=strip["tessera_id"])
    unknown = page(server, narrow, layer=GATED, parent="8589934593")
    assert withheld == unknown == {"artifacts": [], "parents": []}


def test_a_cursor_walks_the_total_order_exactly(browse_server):
    server, _points = browse_server
    token = server.authorise(["1", "2"])["token"]
    whole = page(server, token, layer=SHAPES, limit=200)["artifacts"]
    assert len(whole) > 2, whole
    walked, cursor = [], None
    while True:
        out = page(server, token, layer=SHAPES, limit=1, cursor=cursor)
        assert len(out["artifacts"]) <= 1
        walked.extend(out["artifacts"])
        cursor = out.get("next")
        if cursor is None:
            break
    assert walked == whole, "the one-row walk is the unpaged answer"


def test_a_filter_adds_a_count_per_row_and_moves_nothing_else(browse_server):
    server, points = browse_server
    bound = 1500
    clause = {"fx_key": {"range": {"lt": bound}}}
    for terms in PRINCIPALS:
        token = server.authorise(terms)["token"]
        plain = {r["tessera_id"]: r for r in page(server, token, layer=SHAPES, limit=200)["artifacts"]}
        filtered = page(server, token, layer=SHAPES, filters=clause, limit=200)["artifacts"]
        assert {r["tessera_id"] for r in filtered} == set(plain), (
            f"principal {terms}: a filter served a different set of artifacts (I3, I12)"
        )
        frame = served_artifacts(server, token, SHAPES)
        seen = visible(points, terms)
        any_positive = False
        for row in filtered:
            tid = row["tessera_id"]
            assert row["masked_count"] == plain[tid]["masked_count"], "the masked count moved"
            key = frame[tid].key
            want = sum(
                1
                for p in seen
                if p[0] < bound and key in containing(p[1], p[2]).get(SHAPES, set())
            )
            assert row["matched_count"] == want, f"{terms}/{key}: {row['matched_count']} != {want}"
            any_positive |= want > 0
        assert any_positive, "a filter matching nothing everywhere proves nothing here"
        # Ordered by the filtered count under `filters`.
        keyed = [(-r["matched_count"], int(r["tessera_id"])) for r in filtered]
        assert keyed == sorted(keyed), f"principal {terms}: the filtered order is not total"


def test_the_refusals_are_about_schema_and_the_page_bound_is_published(browse_server):
    server, _points = browse_server
    token = server.authorise(["1", "2"])["token"]
    selection = server.meta(token)["selection"]
    assert selection["max_browse_rows"] > 0

    for body, why in [
        ({"layer": "regions/nowhere"}, "an unknown layer"),
        ({"layer": SHAPES, "level": 0}, "a level on a one-level kind"),
        ({"layer": SHAPES, "limit": 0}, "a zero limit"),
        ({"layer": SHAPES, "parent": "1", "q": "x"}, "two forms at once"),
        ({"layer": SHAPES, "filters": {"nosuch": {"eq": 1}}}, "an unknown filter column"),
    ]:
        resp = server.browse(token, view=VIEW_ID, **body)
        assert resp.status_code == 422, f"{why}: {resp.status_code} {resp.text}"

    # An unknown view is the 404 every other viewer verb gives it.
    assert server.browse(token, view="nope", layer=SHAPES).status_code == 404
    # An identifier that names a point rather than an artifact is an empty page.
    assert page(server, token, layer=SHAPES, parent="8589934593") == {"artifacts": [], "parents": []}
    # `limit` clamps rather than refuses.
    clamped = page(server, token, layer=SHAPES, limit=selection["max_browse_rows"] + 1000)
    assert len(clamped["artifacts"]) <= selection["max_browse_rows"]
