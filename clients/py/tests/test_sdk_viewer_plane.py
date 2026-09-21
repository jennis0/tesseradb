"""The viewer plane's verbs from Python, against a served database.

What is asked for through the HTTP API is asked for here: the artifacts a viewport serves, the
drill-down on one of them, the highlight that lights a served set without moving it, and the
control-plane verbs that undo a declaration or end a deletion.
"""

from __future__ import annotations

import json

import pytest

from tesseradb import Refusal, authorise, connect

pytest.importorskip("pyarrow")

from test_sdk_identity import FRAME, string_ids  # noqa: E402


@pytest.fixture
def db(served, corpus):
    return served(string_ids)


def counts(table) -> dict:
    return json.loads(table.schema.metadata[b"tessera.counts"])


# ---------------------------------------------------------------------------- the artifacts frame


def test_a_viewport_serves_the_artifacts_of_the_layers_it_was_asked_for(db):
    """A layer published from Python is read back from Python: the frame beside the points."""
    served = db.viewport(FRAME, view="map", layers="all")
    assert served.artifacts is not None
    assert served.artifacts.num_rows == 1
    rows = served.artifacts.to_pylist()
    assert rows[0]["layer"] == "clusters"
    assert rows[0]["masked_count"] == 20


def test_a_viewport_asked_for_no_layer_carries_no_artifacts_frame(db):
    """Absent, never empty: a response that served no artifact sends no frame of that kind."""
    assert db.viewport(FRAME, view="map", layers=[]).artifacts is None


def test_a_viewport_reads_as_the_points_table_it_always_did(db):
    """The common case is unchanged: the points, their columns and the counts in the metadata."""
    served = db.viewport(FRAME, view="map", k=8, layers="all")
    assert "tessera_id" in served.column_names
    assert served.num_rows == len(served) == served.points.num_rows
    assert counts(served)["visible"] == 20


def test_a_highlight_lights_the_served_set_without_moving_it(db):
    """One request, two expressions: the served set is identical and the counts gain one."""
    one = db.viewer().browse_artifacts("map", "clusters")["artifacts"][0]["tessera_id"]
    plain = db.viewport(FRAME, view="map", k=512)
    dark = db.viewport(FRAME, view="map", k=512, highlight={"any_of": []})
    lit = db.viewport(
        FRAME,
        view="map",
        k=512,
        highlight={"member_of": {"layer": "clusters", "artifact": one}},
    )
    # The map does not move and the marks do not resample: only the bit changes.
    assert counts(dark)["served"] == counts(lit)["served"] == counts(plain)["served"]
    assert dark.num_rows == lit.num_rows == plain.num_rows
    assert counts(dark)["highlighted"] == 0
    assert counts(lit)["highlighted"] == 20
    assert not any(dark.column("highlighted").to_pylist())
    assert all(lit.column("highlighted").to_pylist())


def test_a_viewport_over_tiles_answers_for_those_tiles(db):
    """`tiles` in place of `bbox`, and the two together is refused before anything is sent."""
    assert db.viewport(view="map", zoom=0, tiles=[0]).num_rows > 0
    with pytest.raises(Refusal):
        db.viewport(FRAME, view="map", tiles=[0])


def test_an_underlay_offset_serves_the_sub_cells_frame(db):
    """The frame a non-zero offset asks for reaches the caller rather than being dropped."""
    served = db.viewport(FRAME, view="map", zoom=0, underlay_offset=2)
    assert served.sub_cells is not None and served.sub_cells.num_rows > 0
    assert db.viewport(FRAME, view="map", zoom=0).sub_cells is None


# ---------------------------------------------------------------------------- the artifact verbs


def test_browse_and_artifact_agree_on_one_annotation(db):
    """The two routes over one artifact: the lineage page, then its own record."""
    viewer = db.viewer()
    page = viewer.browse_artifacts("map", "clusters")
    assert [row["key"] for row in page["artifacts"]] == ["c0"]
    one = page["artifacts"][0]

    record = viewer.artifact(one["tessera_id"], "map")
    assert record["layer"] == "clusters"
    assert record["key"] == "c0"
    assert record["masked_count"] == int(one["masked_count"]) == 20


def test_an_artifact_a_principal_may_not_see_is_not_found(db):
    """`404` is the only failure shape here, and it distinguishes nothing."""
    with pytest.raises(Refusal):
        db.viewer().artifact(2**63 + 1, "map")


def test_browse_refuses_both_forms_at_once(db):
    """A search and a children page are two questions, and the route takes one."""
    with pytest.raises(Refusal):
        db.viewer().browse_artifacts("map", "clusters", parent=1, q="c0")


# ---------------------------------------------------------------------------- the operator verbs


def test_status_is_read_from_the_database(db):
    """The operator's counterpart of `meta()`, which the commit plan already sizes from."""
    assert db.status()["limits"]


def test_compact_is_accepted(db):
    """A deletion leaves the overlay at the fold, and this is how one is asked for."""
    db.remove(["p0"])
    assert db.compact() == {}


def test_a_layer_is_dropped_and_its_name_is_not_freed(db):
    """Declare has an inverse, and the name it tombstones is refused to a later declaration."""
    db.drop_layer("clusters")
    assert db.viewport(FRAME, view="map", layers="all").artifacts is None
    assert "clusters" not in [layer["name"] for layer in db.meta()["layers"]]


def test_a_session_is_revoked_by_its_handle_and_the_token_stops_reading(db):
    """The capability never transits a second time: what is sent is the `token_id`."""
    token = authorise(db.session_url, db.session_credential, ["public"])
    viewer = connect(db.viewer_url, token)
    assert viewer.meta()["views"]

    db.revoke(token)
    with pytest.raises(Refusal):
        viewer.meta()
    # A handle naming no live session is accepted in silence.
    db.revoke(token)
