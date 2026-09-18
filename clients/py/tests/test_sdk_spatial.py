"""A spatial layer, built and then published through the control plane (§4.6, contracts §3.4).

A spatial artifact's shape is its whole membership, resolved per request against each generation's
own segments, so nothing here stages a member set: what is read back is the masked count the shape
produced over the viewer's own visible rows.
"""

from __future__ import annotations

import pyarrow as pa
import pytest

from conftest import browse

from test_sdk_corpus import declare_notebook

pytest.importorskip("pyarrow")

#: The lower-left quarter of the notebook corpus's own coordinates, in the view's space.
REGION = "POLYGON ((0 0, 30000 0, 30000 30000, 0 30000, 0 0))"


def test_a_layer_of_inline_polygons_is_built_and_served_with_a_masked_count(served, corpus):
    def declare(db):
        declare_notebook(db, corpus)
        db.declare_layer(
            "regions",
            kind="flat",
            membership="spatial",
            shape={"kind": "polygon"},
            artifacts=[{"key": "lower_left", "wkt": REGION, "space": "view"}],
            title="Regions",
        )

    db = served(declare)
    assert "[[layer.artifacts]]" in db.declaration
    rows = browse(db, "s0", "regions")["artifacts"]
    assert [row["key"] for row in rows] == ["lower_left"]
    # The corpus holds 50,000 papers and the polygon is a quarter of the frame, so the shape
    # selected some of them and not all of them.
    assert 0 < rows[0]["masked_count"] < 50_000


def test_a_spatial_layer_published_after_the_first_commit_carries_its_wkt_and_its_space(
    served, corpus
):
    db = served(lambda db: declare_notebook(db, corpus))
    db.declare_layer(
        "regions",
        kind="flat",
        membership="spatial",
        shape={"kind": "polygon"},
        title="Regions",
    )
    db.insert(
        "regions",
        artifacts=pa.table(
            {
                "key": pa.array(["lower_left"], pa.string()),
                "wkt": pa.array([REGION], pa.string()),
                "space": pa.array(["view"], pa.string()),
            }
        ),
        key="key",
        wkt="wkt",
        space="space",
    )
    plan = db.check()
    assert plan.ok, plan.output
    assert any("declare layer 'regions'" in line for line in plan.plan), plan.plan
    # A spatial layer's members are never paged: the plan carries the publication and no growth.
    assert not any(line.startswith("grow") for line in plan.plan), plan.plan

    report = db.commit()
    assert report.ok, report
    assert report.artifacts_minted == 1

    rows = browse(db, "s0", "regions")["artifacts"]
    assert [row["key"] for row in rows] == ["lower_left"]
    assert 0 < rows[0]["masked_count"] < 50_000


def test_an_inline_roster_built_at_the_first_commit_is_not_offered_again(served, corpus):
    """An `excluding` row is the case that shows it (ingest §2.3).

    The build compiles the inline roster into the bundle. Offered again at the next commit it
    would be a `409`: the complement is taken over the entities that exist then, so the same list
    is a different set. The first commit records the keys as published, and the plan that follows
    carries no publication for the layer.
    """

    def declare(db):
        declare_notebook(db, corpus)
        db.declare_layer(
            "cohorts",
            kind="flat",
            artifacts=[{"key": "all_but_three", "excluding": [1, 2, 3]}],
            require_member_visibility={"count": 1},
            title="Cohorts",
        )

    db = served(declare)
    rows = browse(db, "s0", "cohorts")["artifacts"]
    assert [row["key"] for row in rows] == ["all_but_three"]
    assert rows[0]["masked_count"] == 49_997

    plan = db.check()
    assert plan.ok, plan
    assert not any("cohorts" in line for line in plan.plan), plan.plan
    report = db.commit()
    assert report.ok, report

    rows = browse(db, "s0", "cohorts")["artifacts"]
    assert rows[0]["masked_count"] == 49_997
