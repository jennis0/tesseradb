"""What `declare_view_group` and a group-scoped block compile to, and what they refuse.

The served proof is `test_sdk_groups.py`; this is the block each parameter writes, asserted as the
TOML the binary reads (python-sdk.md §4.3, §4.5, §4.6).

A group's views and their metadata come from `insert(group, roster=table, key=, …)` and its rows
from `insert(group, table, id=, x=, y=, access=, view=)`, so the roster below is a table and not a
parameter.
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


# ---------------------------------------------------------------------------- the roster


def test_a_roster_is_a_table_and_the_rows_carry_the_view_each_belongs_to(db):
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
    text = db.declaration
    assert '[[view_group]]\nname = "quarter"\ntitle = "By quarter"' in text
    assert 'extent = { x = [-40.0, 40.0], y = [-40.0, 40.0] }' in text
    assert 'point_visibility = { default = "public", field = "access" }' in text
    assert 'metadata = { label = "text" }' in text
    # The group's own points file, and the column saying which view each row lands in.
    assert 'fields = { view = "quarter", x = "x", y = "y", entity_id = "entity_id" }' in text
    # Form B: the roster as a table beside it.
    assert '[view_group.views]\nsource = "quarter_roster"' in text
    assert 'fields = { key = "quarter", label = "label" }' in text
    # A declaration carrying groups alone anchors on the first view of the first roster (0112).
    assert 'allocation_view = "quarter:2026-Q1"' in text


def test_a_members_group_takes_its_own_points_and_declares_no_metadata(db):
    db.declare_view_group("quarter", extent=BOX, metadata={"label": "text"})
    db.declare_view_group("quarter_alt", members="quarter", extent=BOX,
                          projection="web_mercator")
    with pytest.raises(Refusal, match="declares no metadata"):
        db.declare_view_group("third", members="quarter", metadata={"label": "text"})
    text = db.declaration
    assert 'members = "quarter"' in text


def test_a_metadata_name_may_not_be_the_rosters_own_key(db):
    for name in ("key", "source", "visibility"):
        with pytest.raises(Refusal, match="the roster's own key"):
            db.declare_view_group("quarter", extent=BOX, metadata={name: "text"})


def test_a_roster_names_one_column_per_metadata_name_the_group_declared(db):
    db.declare_view_group("quarter", extent=BOX, metadata={"label": "text"})
    insert = db.insert("quarter", roster=roster(["2026-Q1"]), key="quarter", label="label")
    assert insert.metadata_columns == {"label": "label"}
    assert insert.read == ["quarter", "label"]


def test_a_group_and_a_plain_view_share_one_name_space(db):
    """A view of a group is addressed `<group>:<key>` and a plain view by its own name
    (decision 0113), so one name held by both would make a request mean two things."""
    db.declare_view("quarter")
    with pytest.raises(Refusal, match="already declared as a view"):
        db.declare_view_group("quarter", extent=BOX)


# ---------------------------------------------------------------------------- the scoped blocks


def test_a_scoped_attribute_names_its_group_and_its_insert_names_the_view_column(db):
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
    text = db.declaration
    assert 'scope = { group = "quarter" }' in text
    assert 'fields = { view = "quarter" }' in text
    # The id column carries configuration.md's own canonical name, so no `entity_id_field` is
    # written: the default reads it (§1, `[defaults].entity_id_field`).
    assert "entity_id_field" not in text


def test_a_scoped_block_names_a_group_that_is_declared(db):
    for call in (
        lambda: db.declare_attribute("s", type="f32", scope={"group": "quarter"}),
        lambda: db.declare_layer("q", kind="flat", scope={"group": "quarter"}),
    ):
        with pytest.raises(Refusal, match="declare_view_group"):
            call()


def test_a_scoped_layer_with_no_views_is_drawn_on_its_group(db):
    db.declare_view_group("quarter", extent=BOX)
    block = db.declare_layer("q", kind="flat", scope={"group": "quarter"})
    assert block["views"] == ["quarter"]


def test_a_group_sharing_another_s_views_is_declared_after_it(db):
    """views.md §3.3: a group taking another's views is a 404 at the route until it exists."""
    db.declare_view_group("quarter", extent=BOX)
    db.declare_view_group("quarter_alt", members="quarter", extent=BOX)
    names = db.blocks.group_names()
    assert names.index("quarter") < names.index("quarter_alt")
