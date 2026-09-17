"""View groups against a real served database (python-sdk.md §4.3, §4.5, §4.6, §6.2).

The corpus is a frame of a few hundred synthetic points in two views of one group, each view its
own file and its own per-view metadata, with a group-scoped attribute read from those files, a
second one read through a source of its own, and a layer keyed per view. Nothing here doubles the
server: what is under test is that the SDK's declaration is one the build compiles and its pages
are what the control plane takes.
"""

from __future__ import annotations

import datetime as dt

import pyarrow as pa
import pytest

from conftest import browse, viewport
from tesseradb._database import create

pytest.importorskip("pyarrow")

#: The frame both views share, and the whole of it as a viewport.
EXTENT = {"x": [-5.0, 60.0], "y": [-5.0, 60.0]}
FRAME = [-5.0, -5.0, 60.0, 60.0]

N = 200
IDS = list(range(1, N + 1))

STARTS = {
    "a": dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc),
    "b": dt.datetime(2026, 4, 1, tzinfo=dt.timezone.utc),
    "c": dt.datetime(2026, 7, 1, tzinfo=dt.timezone.utc),
}


def points(ids, x0: float) -> pa.Table:
    """One view's points: the same entities laid out elsewhere, with a per-view quality score."""
    return pa.table(
        {
            "entity_id": pa.array(ids, pa.uint64()),
            "x": pa.array([x0 + (i % 20) for i in range(len(ids))], pa.float64()),
            "y": pa.array([float(i % 17) for i in range(len(ids))], pa.float64()),
            "access": pa.array(
                [["alpha"] if i % 2 else ["beta"] for i in range(len(ids))],
                pa.list_(pa.string()),
            ),
            "quality": pa.array([float(i % 10) for i in range(len(ids))], pa.float32()),
        }
    )


def coverage(ids, view: str, value=None) -> pa.Table:
    """The scoped family read through a source of its own: one row per (entity, view)."""
    return pa.table(
        {
            "entity_id": pa.array(list(ids), pa.uint64()),
            "slice": pa.array([view] * len(ids), pa.string()),
            "coverage": pa.array(
                [float(i % 5) if value is None else value for i in range(len(ids))], pa.float32()
            ),
        }
    )


def artifacts(keys, views) -> pa.Table:
    return pa.table(
        {
            "level": pa.array([0] * len(keys), pa.uint32()),
            "key": pa.array(keys, pa.string()),
            "slice": pa.array(views, pa.string()),
        }
    )


def memberships(key: str, view: str, ids) -> pa.Table:
    ids = list(ids)
    return pa.table(
        {
            "level": pa.array([0] * len(ids), pa.uint32()),
            "key": pa.array([key] * len(ids), pa.string()),
            "slice": pa.array([view] * len(ids), pa.string()),
            "entity": pa.array(ids, pa.uint64()),
        }
    )


@pytest.fixture
def grouped(tmp_path, corpus):
    """A committed, served database whose only views are the two views of one group.

    `corpus` is here for the binary it finds; no file of the notebook corpus is read.
    """
    db = create(tmp_path / "db")
    db.stage("slice_a", points(IDS, 0.0))
    db.stage("slice_b", points(IDS, 40.0))
    db.stage("coverage_rows", coverage(IDS, "a"))
    db.stage("clusters_t", artifacts(["c0a", "c0b"], ["a", "b"]))
    db.stage(
        "clusters_m",
        pa.concat_tables([memberships("c0a", "a", IDS), memberships("c0b", "b", IDS)]),
    )
    db.declare_view_group(
        "slices",
        title="Slices",
        extent=EXTENT,
        access="access",
        metadata={"label": "text", "starts": "timestamp_us"},
        views=[
            {"key": "a", "source": "slice_a", "label": "A", "starts": STARTS["a"]},
            {"key": "b", "source": "slice_b", "label": "B", "starts": STARTS["b"]},
        ],
    )
    # Read from each view's own points file: the family is one column per view of the group.
    db.declare_attribute("quality", type="f32", scope={"group": "slices"}, index=True)
    # Read through a source of its own, whose `slice` column says which view each value is for.
    db.declare_attribute(
        "coverage",
        type="f32",
        scope={"group": "slices"},
        index=True,
        source="coverage_rows",
        fields={"view": "slice"},
    )
    db.declare_layer(
        "clusters",
        kind="flat",
        source="clusters_t",
        members="clusters_m",
        scope={"group": "slices"},
        fields={"view": "slice"},
        require_member_visibility="none",
        computed=(),
    )
    report = db.commit()
    assert report.ok, report.output
    yield db
    db.close()


def test_a_groups_views_are_served_with_their_own_metadata(grouped):
    """`/v1/meta` carries the group, its views in roster order, and each view's typed record."""
    meta = grouped.meta()
    assert [group["name"] for group in meta["groups"]] == ["slices"]
    assert meta["groups"][0]["title"] == "Slices"
    assert meta["groups"][0]["views"] == ["slices:a", "slices:b"]
    records = {view["id"]: view for view in meta["views"]}
    assert set(records) == {"slices:a", "slices:b"}
    assert records["slices:a"]["group"] == "slices" and records["slices:a"]["key"] == "a"
    assert records["slices:a"]["metadata"]["label"] == {"type": "text", "value": "A"}
    assert records["slices:b"]["metadata"]["starts"] == {
        "type": "timestamp_us",
        "value": int(STARTS["b"].timestamp() * 1_000_000),
    }
    # The scoped families are published against the group, not as flat columns.
    assert {family["name"] for family in meta.get("scoped_scalars", [])} == {
        "quality",
        "coverage",
    }


def test_each_view_of_a_group_answers_a_viewport(grouped):
    """A view of a group is a view in every respect below the declaration (views.md §3.1), and
    `<group>:<key>` is what a request names it by (decision 0113)."""
    assert viewport(grouped, "slices:a", FRAME)["counts"]["visible"] == N
    assert viewport(grouped, "slices:b", FRAME)["counts"]["visible"] == N
    # A term the corpus staged on half its points, under one view of the group: the mask is the
    # view's own rows, so the count is that half and not the group's.
    half = viewport(grouped, "slices:a", FRAME, terms=["alpha"])
    assert half["counts"]["visible"] == N // 2


def test_a_group_scoped_attribute_filters_inside_the_view_it_was_read_for(grouped):
    """`coverage` is `i % 5` under `slices:a` and undeclared-for-nobody under `slices:b`, which
    the build's own report states; a filter under each view reads that view's own column."""
    under_two = viewport(
        grouped, "slices:a", FRAME, filters={"coverage": {"range": {"lt": 2.0}}}
    )
    assert under_two["counts"]["matched"] == 80
    # The family read from each view's own points file has a value in both.
    for view in ("slices:a", "slices:b"):
        answer = viewport(grouped, view, FRAME, filters={"quality": {"range": {"lt": 2.0}}})
        assert answer["counts"]["matched"] == 40, view


def test_a_scoped_layers_artifacts_are_keyed_per_view(grouped):
    """One artifact set per view of the group: each key is drawn on its own view and no other."""
    on_a = {one["key"]: one["masked_count"] for one in browse(grouped, "slices:a", "clusters")["artifacts"]}
    on_b = {one["key"]: one["masked_count"] for one in browse(grouped, "slices:b", "clusters")["artifacts"]}
    assert on_a["c0a"] == N and on_a.get("c0b", 0) == 0
    assert on_b["c0b"] == N and on_b.get("c0a", 0) == 0


def test_a_later_commit_pages_a_delta_into_one_view_fills_a_family_and_adds_a_view(grouped):
    """§6.2 over a group: the points of one view, a scoped family's values under the view header,
    a scoped layer's artifacts carrying their view, and a view created before the rows that name
    it (views.md §3.2)."""
    db = grouped
    fresh = list(range(9001, 9021))
    added = list(range(9101, 9131))
    db.stage("slice_b", points(fresh, 40.0))
    db.stage("coverage_rows", coverage(IDS[:50], "b", value=1.0))
    db.add_view("slices", "c", source="slice_c", label="C", starts=STARTS["c"])
    db.stage("slice_c", points(added, 20.0))
    db.stage("clusters_t", artifacts(["c1"], ["b"]))
    db.stage("clusters_m", memberships("c1", "b", fresh))

    plan = db.check()
    assert plan.ok, [str(finding) for finding in plan.findings]
    assert plan.plan[0] == "create view 'slices:c' of group 'slices'"
    assert any("points into view 'slices:b'" in line for line in plan.plan)
    assert any("values on existing entities of view 'slices:b'" in line for line in plan.plan)

    report = db.commit()
    assert report.ok, report.refusals
    assert report.rows_accepted == {"slices:b": len(fresh), "slices:c": len(added)}
    assert report.values_filled == 50
    assert report.artifacts_minted == 1

    # The delta landed in the view its header named, and in no other.
    assert viewport(db, "slices:b", FRAME)["counts"]["visible"] == N + len(fresh)
    assert viewport(db, "slices:a", FRAME)["counts"]["visible"] == N
    # The values route filled `slices:b`'s own column, which had none: 50 rows at 1.0.
    filtered = viewport(db, "slices:b", FRAME, filters={"coverage": {"range": {"lt": 2.0}}})
    assert filtered["counts"]["matched"] == 50
    # The view created in this commit answers, with the rows staged for it.
    assert viewport(db, "slices:c", FRAME)["counts"]["visible"] == len(added)
    # The new artifact is drawn on the view its row named and on no other.
    on_b = {one["key"]: one["masked_count"] for one in browse(db, "slices:b", "clusters")["artifacts"]}
    assert on_b["c1"] == len(fresh)
    on_a = {one["key"]: one["masked_count"] for one in browse(db, "slices:a", "clusters")["artifacts"]}
    assert on_a.get("c1", 0) == 0


def test_a_plain_view_declared_after_the_first_commit_is_created_and_served(grouped):
    """`PUT /control/views/{name}` from the emitter's own body (§6.2 step 1). The frame is the
    view's own: there are no rows at a running service to fit one against, so `extent=` is
    written and an `auto` frame would be refused at the route."""
    db = grouped
    db.declare_view("extra", source="slice_d", extent=EXTENT, access="access")
    # A batch into a plain view may not carry a group-scoped family: there is no view of the
    # group for the value to belong to (views.md §5), which the pre-flight refuses by name.
    plain = points(list(range(9201, 9216)), 10.0).drop_columns(["quality"])
    db.stage("slice_d", plain)
    report = db.commit()
    assert report.ok, report.refusals
    assert report.plan[0] == "declare view 'extra'"
    assert report.rows_accepted == {"extra": 15}
    assert viewport(db, "extra", FRAME)["counts"]["visible"] == 15
