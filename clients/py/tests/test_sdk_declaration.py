"""The declaration the verbs build, and what an insert writes onto it.

A `declare_*` verb takes no data, so every source and every column name below was written by an
`insert`. What each declaration means is asserted through the check the build runs over it, and
what an insert wrote is asserted where it was written: the table under `sources/`, and the two
lists the insert prints.
"""

import pyarrow.parquet as pq
import pytest

from tesseradb._database import create
from tesseradb._refusal import Refusal
from tesseradb._toml import Inline, dumps

pd = pytest.importorskip("pandas")


def points(n=4):
    return pd.DataFrame(
        {
            "id": [f"p{i}" for i in range(n)],
            "x": [float(i) for i in range(n)],
            "y": [0.0] * n,
            "cluster": ["a", "b"] * (n // 2),
        }
    )


def members():
    return pd.DataFrame({"key": ["a"], "entity": ["p0"]})


def artifacts():
    return pd.DataFrame({"level": [0], "key": ["a"]})


@pytest.fixture
def db(tmp_path):
    return create(tmp_path / "db")


def test_the_toml_writer_spells_each_shape_once():
    text = dumps(
        {
            "sources": {"points": "sources/points.parquet"},
            "view": [
                {
                    "name": "s0",
                    "extent": Inline({"auto": True, "margin": 0.5}),
                    "members": {"source": "m"},
                    "levels": [{"level": 0, "title": "archive"}],
                }
            ],
        }
    )
    assert "[sources]\npoints = " in text
    assert "[[view]]" in text
    assert 'extent = { auto = true, margin = 0.5 }' in text
    assert "[view.members]\nsource = \"m\"" in text
    assert "[[view.levels]]\nlevel = 0" in text


def test_every_block_names_the_source_and_the_columns_its_inserts_gave_it(db, checked):
    """The file an object reads is on the object, and the column names are the call's."""
    db.declare_view("s0", default_label="public")
    db.declare_attribute("cluster", type="keyword", index=True)
    db.insert("s0", points(), id="id", x="x", y="y")
    assert checked(db).ok
    text = db.declaration
    assert "\nsource = " not in text.split("[[view]]")[0]
    assert text.count('source = "s0"') == 2
    # The id column is the frame's identity rather than one of its values, so no attribute is
    # declared for it and both places that read an id name the column the call named.
    assert 'name = "id"' not in text
    assert 'entity_id = "id"' in text and 'entity_id_field = "id"' in text


def test_a_declared_thing_with_no_insert_is_declared_and_empty(db, checked):
    db.declare_view("s0")
    db.declare_attribute("cluster", type="keyword", index=True)
    assert checked(db).ok
    assert "[sources]" not in db.declaration


def test_the_anchor_view_is_the_allocation_view(db, checked):
    db.declare_view("s0")
    db.declare_view("s1", anchor=True)
    db.insert("s1", points(), id="id", x="x", y="y")
    assert checked(db).ok
    assert 'allocation_view = "s1"' in db.declaration


def test_a_view_names_a_label_for_every_point(db, checked):
    """A view declaring no default reads every point's labels from the column the insert named."""
    db.declare_view("s0", default_label=None)
    db.insert("s0", points(), id="id", x="x", y="y", access="cluster")
    assert checked(db).ok
    assert 'point_visibility = { field = "cluster" }' in db.declaration


def test_a_layer_with_an_artifacts_table_is_closed_and_one_with_a_key_column_is_open(db, checked):
    """Which it is decides whether a key the layer does not declare is minted or refused."""
    db.declare_view("s0")
    db.declare_layer("from_tables", kind="flat")
    db.declare_layer("from_a_column", kind="flat")
    db.insert("s0", points(), id="id", x="x", y="y")
    db.insert("from_tables", artifacts=artifacts(), key="key", level="level")
    db.insert("from_tables", members=members(), id="entity", key="key")
    db.insert("from_a_column", points(), id="id", key="cluster")
    assert checked(db).ok
    text = db.declaration
    assert 'value_set = "closed"' in text and 'value_set = "open"' in text
    assert 'fields = { key = "cluster", entity = "id" }' in text


def test_supplied_content_and_prune_and_depends_on_reach_the_declaration(db, checked):
    db.declare_view("s0")
    db.declare_layer("clusters", kind="flat")
    db.declare_layer(
        "regions",
        kind="flat",
        prune_children=True,
        depends_on=["clusters"],
        supplied=[("name", "text", "inherited")],
        computed=("centroid",),
    )
    db.insert("s0", points(), id="id", x="x", y="y")
    db.insert("clusters", members=members(), id="entity", key="key")
    db.insert("regions", members=members(), id="entity", key="key")
    assert checked(db).ok


def test_a_label_sets_gate_is_one_of_two_words_and_its_tables_come_from_its_inserts(db):
    db.declare_view("s0")
    db.declare_layer("clusters", kind="flat")
    with pytest.raises(Refusal):
        db.declare_labels("t", of="clusters", content_requires="none")
    db.declare_labels("topics/a", of="clusters", content_requires="all")
    db.insert("s0", points(), id="id", x="x", y="y")
    db.insert("clusters", members=members(), id="entity", key="key")
    db.insert("topics/a", {"a": "Diffusion models"})
    db.insert(
        "topics/a",
        members=pd.DataFrame({"key": ["a"], "entity": ["p0"], "rank": [0]}),
        id="entity",
        key="key",
        rank="rank",
    )
    text = db.declaration
    assert "[layer.labels]" in text
    assert '[layer.labels.content]\nrequire_member_visibility = "all"' in text
    assert '[layer.labels.members]\nsource = "topics_a_members"' in text


def test_a_label_set_from_a_mapping_is_written_as_a_key_contents_table(db):
    db.declare_view("s0")
    db.declare_layer("clusters", kind="flat")
    db.declare_labels("topics", of="clusters", content_requires="all")
    insert = db.insert("topics", {"a": "Diffusion models"})
    assert insert.rows == 1
    assert insert.read == ["level", "key", "contents", "attached_layer", "attached_key"]
    assert 'source = "topics"' in db.declaration
    # The SDK wrote the table, so it wrote the attachment: the label hangs from the cluster its
    # key names, in the layer the set was declared `of`.
    written = pq.read_table(insert.path)
    assert written["attached_layer"].to_pylist() == ["clusters"]
    assert written["attached_key"].to_pylist() == ["a"]


def test_a_label_table_the_sdk_reads_as_it_stands_carries_its_own_attachment(db):
    """`contents=` is a table in the publication's shape, so the attachment is in it."""
    db.declare_view("s0")
    db.declare_layer("clusters", kind="flat")
    db.declare_labels("topics", of="clusters", content_requires="all")
    table = pd.DataFrame(
        {
            "key": ["t0"],
            "contents": [[["Diffusion models"]]],
            "attached_layer": ["clusters"],
            "attached_key": ["a"],
        }
    )
    with pytest.raises(Refusal):
        db.insert("topics", table.drop(columns=["attached_layer", "attached_key"]),
                  key="key", contents="contents")
    insert = db.insert(
        "topics", table, key="key", contents="contents",
        attached_layer="attached_layer", attached_key="attached_key",
    )
    assert insert.ignored == []


def test_a_label_table_carries_the_level_and_the_attachment_the_call_named(db):
    """A `text=` table is written by the SDK, and every column the call named travels with it."""
    db.declare_view("s0")
    db.declare_layer("clusters", kind="flat")
    db.declare_labels("topics", of="clusters", content_requires="all")
    insert = db.insert(
        "topics",
        pd.DataFrame({"k": ["t0"], "text": ["Diffusion models"], "lvl": [0], "cluster": ["a"]}),
        key="k",
        text="text",
        level="lvl",
        attached_key="cluster",
    )
    written = pq.read_table(insert.path)
    assert written["key"].to_pylist() == ["t0"]
    assert written["attached_key"].to_pylist() == ["a"]
    assert written["contents"].to_pylist() == [[["Diffusion models"]]]


def test_a_group_takes_its_roster_and_its_rows_from_two_inserts(db, checked):
    db.declare_view_group(
        "quarter",
        metadata={"label": "text"},
        extent={"x": [0.0, 1.0], "y": [0.0, 1.0]},
    )
    db.insert(
        "quarter",
        roster=pd.DataFrame({"quarter": ["q1"], "label": ["Q1"]}),
        key="quarter",
        label="label",
    )
    rows = points()
    rows["quarter"] = ["q1"] * len(rows)
    db.insert("quarter", rows, id="id", x="x", y="y", view="quarter")
    assert checked(db).ok
    text = db.declaration
    assert '[view_group.views]\nsource = "quarter_roster"' in text
    # One `fields` entry per column the call renamed, and none for a column already carrying the
    # name the build reads it under.
    assert 'fields = { key = "quarter", label = "label" }' in text
    assert 'fields = { view = "quarter", entity_id = "id" }' in text


def test_declaring_one_name_twice_is_refused(db):
    db.declare_view("s0")
    with pytest.raises(Refusal):
        db.declare_view("s0")


def test_a_text_table_refuses_a_column_the_written_one_would_not_carry(db):
    """The SDK writes that table, so a column it does not carry is refused, not dropped."""
    db.declare_view("s0")
    db.declare_layer("clusters", kind="flat")
    db.declare_labels("topics", of="clusters", content_requires="all")
    with pytest.raises(Refusal):
        db.insert(
            "topics",
            pd.DataFrame({"k": ["t0"], "text": ["A"], "parent": ["p"]}),
            key="k",
            text="text",
            parent="parent",
        )
