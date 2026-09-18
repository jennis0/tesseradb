"""The declaration the verbs build, and what the TOML always says: python-sdk.md §4.

A `declare_*` verb takes no data, so every `source` and every column name below was written by an
`insert` (§3, §4.8). The TOML is asserted as text rather than parsed back: Python 3.10 carries no
TOML reader, and the declaration is proved to parse by `tessera check` reading it in
`test_sdk_corpus.py`.
"""

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


def test_every_block_names_the_source_and_the_columns_its_inserts_gave_it(db):
    db.declare_view("s0", default_label="public")
    db.declare_attribute("cluster", type="keyword", index=True)
    db.insert("s0", points(), id="id", x="x", y="y")
    text = db.declaration
    assert '[defaults]\nallocation_view = "s0"' in text
    # §4.8: the file each object reads is on the object, and `[defaults].source` is never written.
    assert "\nsource = " not in text.split("[[view]]")[0]
    assert text.count('source = "s0"') == 2
    # And where identity is, since 'id' is not what configuration.md reads it under by default.
    assert 'entity_id = "id"' in text and 'entity_id_field = "id"' in text


def test_a_declared_thing_with_no_insert_is_declared_and_empty(db):
    db.declare_view("s0")
    db.declare_attribute("cluster", type="keyword", index=True)
    text = db.declaration
    assert "[sources]" not in text
    assert "source = " not in text


def test_the_id_column_is_the_identity_rather_than_an_attribute(db):
    """§3: the build takes the column's bytes as the external id, so it is no column of values."""
    db.declare_view("s0")
    db.insert("s0", points(), id="id", x="x", y="y")
    text = db.declaration
    assert 'name = "id"' not in text
    assert 'fields = { x = "x", y = "y", entity_id = "id" }' in text


def test_the_anchor_view_is_the_allocation_view(db):
    db.declare_view("s0")
    db.declare_view("s1", anchor=True)
    assert 'allocation_view = "s1"' in db.declaration
    assert "__anchor__" not in db.declaration


def test_a_view_names_a_label_for_every_point(db):
    db.declare_view("s0", default_label=None)
    db.insert("s0", points(), id="id", x="x", y="y", access="cluster")
    assert 'point_visibility = { field = "cluster" }' in db.declaration


def test_a_key_column_writes_an_open_layer_over_the_column_it_named(db):
    db.declare_view("s0")
    db.declare_layer("clusters", kind="flat")
    db.insert("s0", points(), id="id", x="x", y="y")
    db.insert("clusters", points(), id="id", key="cluster")
    text = db.declaration
    # A layer whose artifacts are minted from a key column is written `open`, without which a key
    # its artifacts do not declare is refused; it reads its own table as `[layer.members]`.
    assert 'value_set = "open"' in text
    assert 'visibility = "public"' in text
    assert 'artifact_visibility = { default = "inherited" }' in text
    assert 'require_member_visibility = "none"' in text
    assert '[layer.members]\nsource = "clusters_key"' in text
    assert 'fields = { key = "cluster", entity = "id" }' in text
    assert 'views = ["s0"]' in text


def test_a_layer_with_an_artifacts_table_is_closed_and_one_with_a_key_column_is_open(db):
    db.declare_view("s0")
    db.declare_layer("from_tables", kind="flat")
    db.declare_layer("from_a_column", kind="flat")
    db.insert("s0", points(), id="id", x="x", y="y")
    db.insert("from_tables", artifacts=artifacts(), key="key", level="level")
    db.insert("from_tables", members=members(), id="entity", key="key")
    db.insert("from_a_column", points(), id="id", key="cluster")
    text = db.declaration
    assert 'value_set = "closed"' in text and 'value_set = "open"' in text


def test_a_levelled_layer_declares_its_levels_and_a_treed_one_is_refused_them(db):
    db.declare_view("s0")
    with pytest.raises(Refusal, match="declares its levels"):
        db.declare_layer("t", kind="tiered")
    with pytest.raises(Refusal, match="its edges"):
        db.declare_layer("n", kind="nested", levels=[(0, "a")])
    db.declare_layer("t", kind="tiered", levels=[(0, "archive"), (1, "class")])
    assert "[[layer.levels]]\nlevel = 0\ntitle = \"archive\"" in db.declaration


def test_supplied_content_and_prune_and_depends_on_reach_the_toml(db):
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
    text = db.declaration
    assert "prune_children = true" in text
    assert 'depends_on = ["clusters"]' in text
    assert "[layer.content]\ncomputed = [\"centroid\"]" in text
    assert '[[layer.content.supplied]]\nname = "name"' in text
    assert 'require_member_visibility = "inherited"' in text


def test_a_label_sets_gate_is_one_of_two_words_and_its_tables_come_from_its_inserts(db):
    db.declare_view("s0")
    db.declare_layer("clusters", kind="flat")
    with pytest.raises(Refusal, match="'all' or 'inherited'"):
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


def test_a_group_takes_its_roster_and_its_rows_from_two_inserts(db):
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
    text = db.declaration
    assert "[[view_group]]" in text
    assert '[view_group.views]\nsource = "quarter_roster"' in text
    assert 'fields = { key = "quarter", label = "label" }' in text
    assert 'fields = { view = "quarter", x = "x", y = "y", entity_id = "id" }' in text


def test_a_category_names_its_vocabulary_and_a_closed_one_takes_its_values(db):
    with pytest.raises(Refusal, match="names its vocabulary"):
        db.declare_attribute("archive", type="category")
    db.declare_vocabulary("archive", closed=True, values=["cs", "math"], width="u8")
    text = db.declaration
    assert 'value_set = "closed"' in text and 'visibility = "public"' in text
    assert 'values = ["cs", "math"]' in text


def test_declaring_one_name_twice_is_refused(db):
    db.declare_view("s0")
    with pytest.raises(Refusal, match="already declared"):
        db.declare_view("s0")
