"""**A membership grows through the control plane** (decision 0127; `artifacts-from-points.md`
§6.1), black-box, over the real binary.

`PATCH /control/layers/{name}/artifacts` adds members to an artifact the level holds, by the key it
was published under. The case publishes one artifact with a third of the corpus and grows it in two
more thirds, then asks the viewer plane for its masked count as each of three principals, and
compares against the fixture's own term model: the count is the number of the corpus's points that
principal may see, computed from `term_of` and nothing served. A growth that dropped a slice, or a
count computed over anything but the union inside the mask, disagrees.

Also pinned: the receipt is a `tessera_id` and the count that joined, with no ordinal and no
membership size; re-sending a slice joins nothing and moves nothing; an unknown key refuses the
whole batch; and a stale idset is a `409` with nothing applied.
"""

from __future__ import annotations

import pytest
import requests

from oracle.harness import spawn_server, stop_server
from oracle.wire import decode_viewport_artifacts, decode_viewport_points

from test_shape_membership import (
    PRINCIPALS,
    VIEW_ID,
    WHOLE_MAP,
    build_bundle,
    fixture_points,
    visible,
)

#: Named without a slash: the route matches one path segment (`test_highlight.py` records the same
#: fact about the publication route).
LAYER = "growth-case"


def _layer() -> dict:
    return {
        "name": LAYER,
        "title": LAYER,
        "views": [VIEW_ID],
        "membership": "enumerated",
        "value_set": "closed",
        "visibility": None,
        "artifact_visibility": {"field": None, "default": "inherited"},
        "require_member_visibility": None,
        "hierarchy": {"kind": "flat", "prune_children": False},
        "content": {"computed": [], "supplied": []},
        "depends_on": [],
        "levels": [],
        "layout": None,
        "shape": None,
    }


def grow(server, **body) -> requests.Response:
    """`PATCH /control/layers/{name}/artifacts` — one growth into one level. The harness carries
    no helper for the verb, so the request is spelled here."""
    return requests.patch(
        f"{server.control_base}/control/layers/{LAYER}/artifacts",
        headers={"Authorization": f"Bearer {server.operator_credential}"},
        json=body,
        timeout=60,
    )


def served_count(server, terms: list[str]) -> int | None:
    """The layer's one artifact's `masked_count` for `terms`, or `None` where it is not served."""
    token = server.authorise(terms)["token"]
    resp = server.viewport_request(token, VIEW_ID, 0, WHOLE_MAP, k=0, layers=[LAYER])
    assert resp.status_code == 200, resp.text
    rows = [a for a in decode_viewport_artifacts(resp.content) if a.layer == LAYER]
    assert len(rows) <= 1, rows
    return rows[0].masked_count if rows else None


@pytest.fixture(scope="module")
def growth_server(tmp_path_factory):
    points = fixture_points()
    work = tmp_path_factory.mktemp("growth-fixture")
    bundle = build_bundle(work, points)
    server, proc = spawn_server(
        bundle,
        tmp_path_factory.mktemp("growth-server"),
        max_k=100_000,
        k_max_marks=100_000,
        theta_target_marks=10**12,
    )
    yield server, points
    stop_server(proc)


def test_a_membership_grown_in_slices_serves_the_union_inside_each_principals_mask(growth_server):
    server, points = growth_server
    token = server.authorise(["1", "2"])["token"]
    idset = server.meta(token)["idset"]
    resp = server.viewport_request(token, VIEW_ID, 0, WHOLE_MAP, k=100_000)
    assert resp.status_code == 200, resp.text
    table = decode_viewport_points(resp.content)
    ids = [str(i) for i in table.column("tessera_id").to_pylist()]
    assert len(ids) == len(points), "the all-seeing principal is served the whole corpus"
    third = len(ids) // 3
    slices = [ids[:third], ids[third : 2 * third], ids[2 * third :]]

    resp = server.register_layer(_layer())
    assert resp.status_code == 201, resp.text
    resp = server.publish_artifacts(
        LAYER, addressing="tessera", idset=idset, artifacts=[{"key": "whole", "members": slices[0]}]
    )
    assert resp.status_code == 201, resp.text
    tessera_id = resp.json()["artifacts"][0]["tessera_id"]

    for joining in slices[1:]:
        resp = grow(
            server, addressing="tessera", idset=idset, artifacts=[{"key": "whole", "members": joining}]
        )
        assert resp.status_code == 200, resp.text
        (row,) = resp.json()["artifacts"]
        assert (row["key"], row["tessera_id"], row["joined"]) == ("whole", tessera_id, len(joining)), row

    for terms in PRINCIPALS:
        expected = len(visible(points, terms))
        assert served_count(server, terms) == expected, terms

    # Re-sending a slice names the artifact and adds nothing.
    resp = grow(server, addressing="tessera", idset=idset, artifacts=[{"key": "whole", "members": slices[1]}])
    assert resp.status_code == 200, resp.text
    assert resp.json()["artifacts"][0]["joined"] == 0
    assert served_count(server, ["1", "2"]) == len(points)


def test_an_unknown_key_and_a_stale_idset_each_refuse_the_batch_with_nothing_applied(growth_server):
    server, points = growth_server
    token = server.authorise(["1", "2"])["token"]
    idset = server.meta(token)["idset"]
    before = served_count(server, ["1", "2"])
    assert before is not None, "the first test published the artifact"
    resp = server.viewport_request(token, VIEW_ID, 0, WHOLE_MAP, k=100_000)
    ids = [str(i) for i in decode_viewport_points(resp.content).column("tessera_id").to_pylist()]

    resp = grow(
        server,
        addressing="tessera",
        idset=idset,
        artifacts=[
            {"key": "whole", "members": ids[:5]},
            {"key": "never-published", "members": ids[5:10]},
        ],
    )
    assert resp.status_code == 422, resp.text
    assert "never-published" in resp.text

    resp = grow(server, addressing="tessera", idset=idset + 1, artifacts=[{"key": "whole", "members": ids[:5]}])
    assert resp.status_code == 409, resp.text

    assert served_count(server, ["1", "2"]) == before
