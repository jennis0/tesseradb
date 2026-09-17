"""What `declare_view_group`, a group-scoped block and `add_view` compile to, and what they refuse.

The served proof is `test_sdk_groups.py`; this is the block each parameter writes, asserted as the
TOML the binary reads (python-sdk.md §4.3, §4.5, §4.6).
"""

import datetime as dt

import pytest

from tesseradb._database import create
from tesseradb._refusal import Refusal

pytest.importorskip("pyarrow")

BOX = {"x": [-40.0, 40.0], "y": [-40.0, 40.0]}


@pytest.fixture
def db(tmp_path):
    import pyarrow as pa

    database = create(tmp_path / "db")
    for name in ("q1", "q2", "all_quarters", "scoped_values", "clusters"):
        # Each carries the access column its group names: a view whose points file does not
        # carry its labels is refused naming the column (§4.2).
        database.stage(
            name,
            pa.table(
                {
                    "entity_id": pa.array([1], pa.uint64()),
                    "access": pa.array([["public"]], pa.list_(pa.string())),
                }
            ),
        )
    return database


# ---------------------------------------------------------------------------- the three rosters


def test_form_a_writes_one_block_per_view_each_naming_its_own_file(db):
    db.declare_view_group(
        "quarter",
        title="By quarter",
        extent=BOX,
        access="access",
        metadata={"label": "text", "starts": "timestamp_us"},
        views=[
            {"key": "2026-Q1", "source": "q1", "label": "Q1",
             "starts": dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc)},
            {"key": "2026-Q2", "source": "q2", "label": "Q2", "visibility": "staff",
             "starts": dt.datetime(2026, 4, 1, tzinfo=dt.timezone.utc)},
        ],
    )
    text = db.declaration
    assert '[[view_group]]\nname = "quarter"' in text
    assert 'title = "By quarter"' in text and 'projection = "none"' in text
    assert "extent = { x = [-40.0, 40.0], y = [-40.0, 40.0] }" in text
    assert 'point_visibility = { field = "access", default = "public" }' in text
    assert 'visibility = "public"' in text
    assert 'metadata = { label = "text", starts = "timestamp_us" }' in text
    assert '[[view_group.view]]\nkey = "2026-Q1"\nsource = "q1"' in text
    assert 'label = "Q1"\nstarts = 2026-01-01T00:00:00Z' in text
    assert 'visibility = "staff"' in text
    # A group is the whole of the declaration's views here, so the anchor is a view of it.
    assert 'allocation_view = "quarter:2026-Q1"' in text
    # The group declares no source of its own: the file is the view.
    assert "\nsource = " not in text.split("[[view_group]]")[1].split("[[view_group.view]]")[0]


def test_one_source_and_a_view_field_write_the_discriminator(db):
    db.declare_view_group(
        "quarter",
        source="all_quarters",
        view_field="quarter",
        extent=BOX,
        access="access",
        projection="web_mercator",
    )
    text = db.declaration
    assert 'source = "all_quarters"' in text and 'fields = { view = "quarter" }' in text
    assert 'projection = "web_mercator"' in text
    assert "[[view_group.view]]" not in text


def test_a_members_group_takes_its_own_points_and_declares_no_roster(db):
    db.declare_view_group("quarter", source="all_quarters", view_field="quarter", extent=BOX,
                          access="access")
    db.declare_view_group(
        "quarter_map",
        members="quarter",
        source="all_quarters",
        view_field="quarter",
        extent=BOX,
        access="access",
    )
    assert 'members = "quarter"' in db.declaration


def test_a_group_refuses_the_rosters_that_contradict_each_other(db):
    with pytest.raises(Refusal, match="declares no roster and no metadata"):
        db.declare_view_group("g", members="quarter", views=[{"key": "a", "source": "q1"}])
    with pytest.raises(Refusal, match="declares no roster and no metadata"):
        db.declare_view_group("g", members="quarter", metadata={"label": "text"},
                              source="all_quarters", view_field="q")
    with pytest.raises(Refusal, match="the roster decides where the points come from"):
        db.declare_view_group("g", views=[{"key": "a", "source": "q1"}], source="all_quarters",
                              view_field="q", extent=BOX)
    with pytest.raises(Refusal, match="source= and view_field= together"):
        db.declare_view_group("g", source="all_quarters", extent=BOX)
    with pytest.raises(Refusal, match="a group's points come from"):
        db.declare_view_group("g", extent=BOX)


def test_a_roster_record_carries_every_metadata_name_and_no_other(db):
    with pytest.raises(Refusal, match="never be supplied. Give starts"):
        db.declare_view_group("g", extent=BOX, metadata={"starts": "timestamp_us"},
                              views=[{"key": "a", "source": "q1"}])
    with pytest.raises(Refusal, match="not a metadata name this group declares"):
        db.declare_view_group("g", extent=BOX, views=[{"key": "a", "source": "q1", "label": "A"}])
    with pytest.raises(Refusal, match="the roster's own key"):
        db.declare_view_group("g", extent=BOX, metadata={"visibility": "text"},
                              views=[{"key": "a", "source": "q1", "visibility": "x"}])
    with pytest.raises(Refusal, match="names its key"):
        db.declare_view_group("g", extent=BOX, views=[{"source": "q1"}])
    with pytest.raises(Refusal, match="the file is the view"):
        db.declare_view_group("g", extent=BOX, views=[{"key": "a"}])


def test_a_group_and_a_plain_view_share_one_name_space(db):
    db.declare_view("quarter", source="q1")
    with pytest.raises(Refusal, match="already declared as a view"):
        db.declare_view_group("quarter", source="all_quarters", view_field="q", extent=BOX)
    db.declare_view_group("slices", source="all_quarters", view_field="q", extent=BOX)
    with pytest.raises(Refusal, match="already declared as a view group"):
        db.declare_view("slices", source="q1")


# ---------------------------------------------------------------------------- add_view


def test_a_view_added_to_a_group_is_a_roster_record(db):
    db.declare_view_group("quarter", extent=BOX, metadata={"label": "text"},
                          views=[{"key": "a", "source": "q1", "label": "A"}])
    db.add_view("quarter", "b", source="q2", label="B")
    text = db.declaration
    assert '[[view_group.view]]\nkey = "b"\nsource = "q2"\nlabel = "B"' in text
    with pytest.raises(Refusal, match="already declared"):
        db.add_view("quarter", "b", source="q2", label="B")
    with pytest.raises(Refusal, match="never be supplied"):
        db.add_view("quarter", "c", source="q2")


def test_a_key_belongs_to_the_group_that_owns_it(db):
    db.declare_view_group("quarter", source="all_quarters", view_field="q", extent=BOX)
    db.declare_view_group("quarter_map", members="quarter", source="all_quarters",
                          view_field="q", extent=BOX)
    with pytest.raises(Refusal, match="Add the view to 'quarter'"):
        db.add_view("quarter_map", "a", source="q1")
    with pytest.raises(Refusal, match="would be a second roster"):
        db.add_view("quarter", "a", source="q1")


# ---------------------------------------------------------------------------- scoped blocks


def test_a_scoped_attribute_names_its_group_and_where_its_view_column_is(db):
    db.declare_view_group("quarter", extent=BOX, views=[{"key": "a", "source": "q1"}])
    db.declare_attribute("sentiment", type="f32", scope={"group": "quarter"}, index=True)
    db.declare_attribute("coverage", type="f32", scope={"group": "quarter"}, index=True,
                         source="scoped_values", fields={"view": "quarter"})
    text = db.declaration
    assert 'name = "sentiment"\ntype = "f32"\nscope = { group = "quarter" }' in text
    assert 'fields = { view = "quarter" }' in text
    # A scoped column with no source of its own is read from each view's points file, so
    # `[defaults].source` does not reach it (configuration.md §1).
    assert 'source = "scoped_values"' in text
    assert "sentiment" in text and text.count('source = "points"') == 0


def test_a_scoped_block_names_a_group_that_is_declared(db):
    with pytest.raises(Refusal, match="which this declaration does not carry"):
        db.declare_attribute("sentiment", type="f32", scope={"group": "quarter"})
    with pytest.raises(Refusal, match="which this declaration does not carry"):
        db.declare_layer("clusters", kind="flat", source="clusters", members="clusters",
                         scope={"group": "quarter"}, fields={"view": "quarter"})
    with pytest.raises(Refusal, match="names one view group"):
        db.declare_attribute("sentiment", type="f32", scope={"group": "q", "extra": 1})


def test_a_scoped_block_with_its_own_source_names_its_view_column(db):
    db.declare_view_group("quarter", extent=BOX, views=[{"key": "a", "source": "q1"}])
    with pytest.raises(Refusal, match="Give fields=\\{'view': column\\}"):
        db.declare_attribute("coverage", type="f32", scope={"group": "quarter"},
                             source="scoped_values")
    with pytest.raises(Refusal, match="its rows carry the view"):
        db.declare_layer("clusters", kind="flat", source="clusters", members="clusters",
                         scope={"group": "quarter"})


def test_a_scoped_layer_with_no_views_is_drawn_on_its_group(db):
    db.declare_view_group("quarter", extent=BOX, views=[{"key": "a", "source": "q1"}])
    db.declare_layer("clusters", kind="flat", source="clusters", members="clusters",
                     scope={"group": "quarter"}, fields={"view": "quarter"})
    assert 'views = ["quarter"]' in db.declaration


def test_a_scoped_layer_takes_no_from_column(db):
    """Every published record carries the view it belongs to (contracts §3.4 r84), and one key
    per point says nothing about which view; the refusal names the tables that do."""
    db.declare_view_group("quarter", extent=BOX, views=[{"key": "a", "source": "q1"}])
    for built in (False, True):
        db.built = built
        with pytest.raises(Refusal, match="Declare it with source= and members="):
            db.declare_layer("clusters", kind="flat", from_column="cluster",
                             scope={"group": "quarter"}, fields={"view": "quarter"})


def test_a_group_sharing_another_s_views_is_declared_after_it():
    """A group naming `members` is a 404 at the route until the group it names exists, whatever
    order the declaration is in (views.md §3.3)."""
    from tesseradb._commit import _owners_first

    groups = [
        {"name": "quarter_map", "body": {"members": "quarter"}},
        {"name": "quarter", "body": {}},
        {"name": "region", "body": {}},
    ]
    assert [one["name"] for one in _owners_first(groups)] == ["quarter", "quarter_map", "region"]
