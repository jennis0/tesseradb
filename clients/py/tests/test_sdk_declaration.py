"""The declaration the verbs build, and what the TOML always says: python-sdk.md §4.

The TOML is asserted as text rather than parsed back: Python 3.10 carries no TOML reader, and the
declaration is proved to parse by `tessera check` reading it in `test_sdk_corpus.py`.
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


@pytest.fixture
def db(tmp_path):
    database = create(tmp_path / "db")
    database.stage("points", points(), id="id", default=True)
    return database


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


def test_every_block_names_its_source_and_the_defaults_name_the_allocation_view(db):
    db.declare_view("s0", access=None)
    db.declare_attribute("cluster", type="keyword", index=True)
    text = db.declaration
    assert '[defaults]\nsource = "points"\nallocation_view = "s0"' in text
    # A view and an attribute that named no source read `[defaults].source`, and the SDK writes it
    # out on both, so a reader sees the whole declaration.
    # The view, the attribute, and the keyword attribute the SDK keeps for the user's own id.
    assert text.count('source = "points"') == 3 + 1


def test_the_users_id_column_is_kept_as_an_indexed_keyword_attribute(db):
    db.declare_view("s0")
    text = db.declaration
    assert '[[attribute]]\nname = "id"\ntype = "keyword"\nsource = "points"\nindex = true' in text


def test_an_explicit_declaration_overrides_the_inferred_block(db):
    db.declare_view("s0")
    db.declare_attribute("id", type="text", index=True)
    assert db.declaration.count('name = "id"') == 1
    assert 'type = "keyword"' not in db.declaration


def test_a_view_with_no_source_and_no_default_is_refused_naming_the_block(tmp_path):
    database = create(tmp_path / "db")
    database.stage("points", points(), id="id")
    database.declare_view("s0")
    with pytest.raises(Refusal, match="view 's0' names no source"):
        database.declaration


def test_the_anchor_view_is_the_allocation_view(db):
    db.stage("second", points(), id="id")
    db.declare_view("s0")
    db.declare_view("s1", source="second", anchor=True)
    assert 'allocation_view = "s1"' in db.declaration
    assert "__anchor__" not in db.declaration


def test_a_view_names_a_label_for_every_point(db):
    with pytest.raises(Refusal, match="names no label"):
        db.declare_view("s0", default_label=None)
    db.declare_view("s0", access="terms", default_label=None)
    assert 'point_visibility = { field = "terms" }' in db.declaration


def test_a_layer_always_writes_its_value_set_and_its_three_disclosure_controls(db):
    db.declare_view("s0")
    db.declare_layer("clusters", kind="flat", from_column="cluster")
    text = db.declaration
    # A from-column layer is written `open`, without which a key its artifacts do not declare is
    # refused, and it compiles to `[layer.members]` over the points source.
    assert 'value_set = "open"' in text
    assert 'visibility = "public"' in text
    assert 'artifact_visibility = { default = "inherited" }' in text
    assert 'require_member_visibility = "none"' in text
    assert '[layer.members]\nsource = "points"' in text
    assert 'fields = { key = "cluster", entity = "entity_id" }' in text
    assert 'views = ["s0"]' in text


def test_a_layer_from_tables_is_closed_and_a_members_only_layer_is_open(db):
    db.stage("artifacts", pd.DataFrame({"level": [0], "key": ["a"]}))
    db.stage("members", pd.DataFrame({"key": ["a"], "entity": [1]}))
    db.declare_view("s0")
    db.declare_layer("from_tables", kind="flat", source="artifacts", members="members")
    db.declare_layer("members_only", kind="flat", members="members")
    text = db.declaration
    assert 'value_set = "closed"' in text and 'value_set = "open"' in text


def test_a_levelled_layer_declares_its_levels_and_a_treed_one_is_refused_them(db):
    db.stage("members", pd.DataFrame({"key": ["a"], "entity": [1]}))
    db.declare_view("s0")
    with pytest.raises(Refusal, match="declares its levels"):
        db.declare_layer("t", kind="tiered", members="members")
    with pytest.raises(Refusal, match="its edges"):
        db.declare_layer("n", kind="nested", members="members", levels=[(0, "a")])
    db.declare_layer("t", kind="tiered", members="members", levels=[(0, "archive"), (1, "class")])
    assert "[[layer.levels]]\nlevel = 0\ntitle = \"archive\"" in db.declaration


def test_a_layer_with_no_membership_route_is_refused(db):
    db.declare_view("s0")
    with pytest.raises(Refusal, match="from_column"):
        db.declare_layer("nothing", kind="flat")


def test_supplied_content_and_prune_and_depends_on_reach_the_toml(db):
    db.stage("members", pd.DataFrame({"key": ["a"], "entity": [1]}))
    db.declare_view("s0")
    db.declare_layer("clusters", kind="flat", members="members")
    db.declare_layer(
        "regions",
        kind="flat",
        members="members",
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


def test_a_label_sets_two_gates_are_the_pairings_the_design_allows(db):
    db.stage("members", pd.DataFrame({"key": ["a"], "entity": [1]}))
    db.stage("topics", pd.DataFrame({"level": [0], "key": ["a"]}))
    db.declare_view("s0")
    db.declare_layer("clusters", kind="flat", members="members")
    with pytest.raises(Refusal, match="names the generating set"):
        db.declare_labels("t", of="clusters", source="topics", content_requires="all")
    with pytest.raises(Refusal, match="declares no generating set"):
        db.declare_labels(
            "t", of="clusters", source="topics", members="members", content_requires="inherited"
        )
    with pytest.raises(Refusal, match="'all' or 'inherited'"):
        db.declare_labels("t", of="clusters", source="topics", content_requires="none")
    # The default follows `members`: `all` when given, `inherited` when not.
    db.declare_labels("topics/a", of="clusters", source="topics", members="members")
    text = db.declaration
    assert "[layer.labels]" in text
    assert '[layer.labels.content]\nrequire_member_visibility = "all"' in text
    assert '[layer.labels.members]\nsource = "members"' in text


def test_a_label_set_from_a_mapping_is_written_as_a_key_contents_table(db):
    db.stage("members", pd.DataFrame({"key": ["a"], "entity": [1]}))
    db.declare_view("s0")
    db.declare_layer("clusters", kind="flat", members="members")
    db.declare_labels("topics", of="clusters", source={"a": "Diffusion models"})
    assert db.sources["topics"].rows == 1
    assert 'source = "topics"' in db.declaration


def test_form_bs_roster_table_goes_through_the_generic_form(db):
    """Form A, the one-source form and `members` are `declare_view_group`'s three rosters; the
    roster as a table has no parameter and is written as the block it is."""
    db.declare_view("s0")
    db.declare("view_group", {
        "name": "quarter",
        "source": "points",
        "fields": Inline({"view": "quarter"}),
        "extent": Inline({"x": [0.0, 1.0], "y": [0.0, 1.0]}),
        "point_visibility": Inline({"default": "public"}),
        "views": {"source": "roster", "fields": Inline({"key": "quarter"})},
    })
    text = db.declaration
    assert "[[view_group]]" in text
    assert '[view_group.views]\nsource = "roster"' in text


def test_a_category_names_its_vocabulary_and_a_closed_one_names_its_values(db):
    with pytest.raises(Refusal, match="names its vocabulary"):
        db.declare_attribute("archive", type="category")
    with pytest.raises(Refusal, match="closed value set"):
        db.declare_vocabulary("archive", closed=True)
    db.declare_vocabulary("archive", closed=True, values=["cs", "math"], width="u8")
    text = db.declaration
    assert 'value_set = "closed"' in text and 'visibility = "public"' in text
    assert 'values = ["cs", "math"]' in text


def test_declaring_one_name_twice_is_refused(db):
    db.declare_view("s0")
    with pytest.raises(Refusal, match="already declared"):
        db.declare_view("s0")
