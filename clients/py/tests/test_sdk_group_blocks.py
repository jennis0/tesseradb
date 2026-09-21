"""What `declare_view_group` and a group-scoped block write, and what they refuse.

A group's views and their metadata come from `insert(group, roster=table, key=, …)` and its rows
from `insert(group, table, id=, x=, y=, access=, view=)`, so a roster here is a table and not a
parameter. Both roster forms are declared and handed to the check; the served proof is
`test_sdk_groups.py`.
"""

import pytest

from tesseradb._database import create
from tesseradb._refusal import Refusal

pa = pytest.importorskip("pyarrow")

BOX = {"x": [-40.0, 40.0], "y": [-40.0, 40.0]}


def roster(keys, labels=None):
    return pa.table(
        {
            "quarter": pa.array(keys, pa.string()),
            "label": pa.array(labels or [key.upper() for key in keys], pa.string()),
        }
    )


def rows(keys):
    return pa.table(
        {
            "entity_id": pa.array(list(range(len(keys))), pa.uint64()),
            "x": pa.array([0.0] * len(keys), pa.float64()),
            "y": pa.array([0.0] * len(keys), pa.float64()),
            "access": pa.array([["public"]] * len(keys), pa.list_(pa.string())),
            "quarter": pa.array(keys, pa.string()),
        }
    )


@pytest.fixture
def db(tmp_path):
    return create(tmp_path / "db")


# ---------------------------------------------------------------------------- the two rosters


def test_a_roster_is_a_table_and_the_rows_carry_the_view_each_belongs_to(db, checked):
    db.declare_view_group("quarter", title="By quarter", extent=BOX, metadata={"label": "text"})
    db.insert("quarter", roster=roster(["2026-Q1", "2026-Q2"]), key="quarter", label="label")
    db.insert(
        "quarter",
        rows(["2026-Q1", "2026-Q2"]),
        id="entity_id",
        x="x",
        y="y",
        access="access",
        view="quarter",
    )
    assert checked(db).ok
    # A declaration carrying groups alone anchors on the first view of the first roster.
    assert 'allocation_view = "quarter:2026-Q1"' in db.declaration


def test_a_group_whose_views_each_have_their_own_file_inserts_one_table_per_view(db, checked):
    """The other roster: `view_key=` names the one view a whole table is for."""
    db.declare_view_group("quarter", extent=BOX, metadata={"label": "text"})
    db.insert("quarter", roster=roster(["q1", "q2"]), key="quarter", label="label")
    for key in ("q1", "q2"):
        db.insert("quarter", rows([key]), id="entity_id", x="x", y="y", access="access",
                  view_key=key)
    assert checked(db).ok
    assert db.declaration.count("[[view_group.view]]") == 2
    assert 'allocation_view = "quarter:q1"' in db.declaration


def test_a_members_group_takes_its_own_points_and_declares_no_metadata(db, checked):
    db.declare_view_group("quarter", extent=BOX, metadata={"label": "text"})
    db.declare_view_group("quarter_alt", members="quarter", projection="web_mercator",
                          extent={"lon": [-180.0, 180.0], "lat": [-85.05, 85.05]})
    with pytest.raises(Refusal):
        db.declare_view_group("third", members="quarter", metadata={"label": "text"})
    db.insert("quarter", roster=roster(["q1"]), key="quarter", label="label")
    db.insert("quarter", rows(["q1"]), id="entity_id", x="x", y="y", access="access",
              view="quarter")
    assert checked(db).ok


def test_a_roster_names_one_column_per_metadata_name_the_group_declared(db):
    db.declare_view_group("quarter", extent=BOX, metadata={"label": "text"})
    insert = db.insert("quarter", roster=roster(["2026-Q1"]), key="quarter", label="label")
    assert insert.metadata_columns == {"label": "label"}
    assert insert.read == ["quarter", "label"]


def test_a_group_and_a_plain_view_share_one_name_space(db):
    """A view of a group is addressed `<group>:<key>` and a plain view by its own name, so one
    name held by both would make a request mean two things."""
    db.declare_view("quarter")
    with pytest.raises(Refusal):
        db.declare_view_group("quarter", extent=BOX)


# ---------------------------------------------------------------------------- the scoped blocks


def test_a_scoped_attribute_names_its_group_and_its_insert_names_the_view_column(db, checked):
    db.declare_view_group("quarter", extent=BOX)
    db.declare_attribute("sentiment", type="f32", scope={"group": "quarter"}, index=True)
    db.insert(
        "quarter",
        rows(["2026-Q1"]),
        id="entity_id",
        x="x",
        y="y",
        access="access",
        view="quarter",
    )
    db.insert(
        "sentiment",
        pa.table(
            {
                "entity_id": pa.array([0], pa.uint64()),
                "quarter": pa.array(["2026-Q1"], pa.string()),
                "sentiment": pa.array([0.5], pa.float32()),
            }
        ),
        id="entity_id",
        value="sentiment",
        view="quarter",
    )
    assert checked(db).ok
    assert 'scope = { group = "quarter" }' in db.declaration


def test_a_scoped_block_names_a_group_that_is_declared(db):
    for call in (
        lambda: db.declare_attribute("s", type="f32", scope={"group": "quarter"}),
        lambda: db.declare_layer("q", kind="flat", scope={"group": "quarter"}),
    ):
        with pytest.raises(Refusal):
            call()


def test_a_scoped_layer_with_no_views_is_drawn_on_its_group(db, checked):
    db.declare_view_group("quarter", extent=BOX)
    db.declare_layer("q", kind="flat", scope={"group": "quarter"})
    db.insert("quarter", roster=roster(["q1"]), key="quarter")
    db.insert("quarter", rows(["q1"]), id="entity_id", x="x", y="y", access="access",
              view="quarter")
    assert checked(db).ok
    assert 'views = ["quarter"]' in db.declaration


def test_a_group_sharing_another_s_views_is_declared_after_it(db):
    """A group taking another's views is a 404 at the route until that group exists."""
    db.declare_view_group("quarter", extent=BOX)
    db.declare_view_group("quarter_alt", members="quarter", extent=BOX)
    names = db.blocks.group_names()
    assert names.index("quarter") < names.index("quarter_alt")


# ---------------------------------------------------------------------------- the refusals


def test_a_roster_refuses_a_keyword_that_is_no_metadata_name(db):
    """A roster is read like every other table: the names are the group's, or they are nothing."""
    db.declare_view_group("quarter", extent=BOX, metadata={"label": "text"})
    with pytest.raises(Refusal):
        db.insert("quarter", roster=roster(["2026-Q1"]), key="quarter", colour="label")


def test_a_roster_column_the_call_did_not_name_is_refused(db):
    """`key` and `visibility` are the roster's own, so a table carrying one names it."""
    db.declare_view_group("quarter", extent=BOX, metadata={"label": "text"})
    table = pa.table(
        {
            "key": pa.array(["2026-Q1"], pa.string()),
            "label": pa.array(["Q1"], pa.string()),
            "visibility": pa.array(["public"], pa.string()),
        }
    )
    with pytest.raises(Refusal):
        db.insert("quarter", roster=table, key="key", label="label")
    insert = db.insert(
        "quarter", roster=table, key="key", label="label", visibility="visibility"
    )
    assert insert.ignored == []


def test_a_groups_rows_name_the_view_one_way_or_the_other(db):
    db.declare_view_group("quarter", extent=BOX)
    with pytest.raises(Refusal):
        db.insert("quarter", rows(["q1"]), id="entity_id", x="x", y="y", access="access")
    with pytest.raises(Refusal):
        db.insert("quarter", rows(["q1"]), id="entity_id", x="x", y="y", access="access",
                  view="quarter", view_key="q1")
