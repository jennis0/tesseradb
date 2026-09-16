"""The rest of the layer and vocabulary surface: python-sdk.md §4.4 and §4.6.

What each parameter compiles to, and what each refusal says. The blocks are read back from the
verb's own return value; the TOML they become is proved by `tessera check` in `test_sdk_corpus.py`
and against every committed corpus declaration in `test_sdk_corpora.py`.
"""

import pytest

from tesseradb._database import create
from tesseradb._refusal import Refusal
from tesseradb._toml import Inline

pd = pytest.importorskip("pandas")


@pytest.fixture
def db(tmp_path):
    database = create(tmp_path / "db")
    database.stage(
        "points",
        pd.DataFrame({"id": ["p0", "p1"], "x": [0.0, 1.0], "y": [0.0, 1.0]}),
        id="id",
        default=True,
    )
    database.stage("members", pd.DataFrame({"key": ["a"], "entity": [1]}))
    database.stage("shapes", pd.DataFrame({"key": ["a"], "wkt": ["POLYGON ((0 0, 1 0, 1 1, 0 0))"]}))
    database.declare_view("s0")
    return database


# ---------------------------------------------------------------------------- membership


def test_a_spatial_layer_names_its_shape_kind_and_the_space_its_shapes_are_written_in(db):
    block = db.declare_layer(
        "regions",
        kind="flat",
        source="shapes",
        membership="spatial",
        shape={"kind": "polygon"},
        default_space="wgs84",
    )
    assert block["membership"] == "spatial"
    assert block["shape"] == {"kind": "polygon"}
    assert block["default_space"] == "wgs84"
    text = db.declaration
    assert '[layer.shape]\nkind = "polygon"' in text
    assert 'default_space = "wgs84"' in text


def test_a_spatial_layer_derives_no_hull_beside_its_membership_shape(db):
    block = db.declare_layer("regions", kind="flat", source="shapes", membership="spatial",
                             shape="bbox")
    assert block["content"] == Inline({"computed": ["centroid", "box"]})
    with pytest.raises(Refusal, match="one drawn geometry"):
        db.declare_layer("more", kind="flat", source="shapes", membership="spatial", shape="bbox",
                         computed=("centroid", "hull"))


def test_a_shape_kind_may_be_named_bare(db):
    block = db.declare_layer("regions", kind="flat", source="shapes", membership="spatial",
                             shape="bbox")
    assert block["shape"] == {"kind": "bbox"}


def test_an_attribute_membership_is_a_predicate_and_declares_no_content(db):
    block = db.declare_layer("programmes", kind="flat", membership={"attribute": "kind"})
    assert block["membership"] == Inline({"attribute": "kind"})
    # A predicate layer's artifacts are the column's distinct values, which the vocabulary bounds.
    assert block["value_set"] == "closed"
    assert "content" not in block
    assert "[layer.content]" not in db.declaration


def test_a_layer_scoped_to_a_view_group_keys_its_artifacts_per_view(db):
    db.declare("view_group", {"name": "quarter", "source": "points"})
    block = db.declare_layer(
        "quarter_clusters",
        kind="flat",
        source="members",
        views=["quarter"],
        scope={"group": "quarter"},
        fields={"view": "quarter"},
    )
    assert block["scope"] == Inline({"group": "quarter"})
    assert block["fields"] == Inline({"view": "quarter"})


# ---------------------------------------------------------------------------- the artifact rows


def test_inline_artifacts_carry_the_shape_the_space_and_the_exclusion(db):
    block = db.declare_layer(
        "regions",
        kind="flat",
        membership="spatial",
        shape={"kind": "polygon"},
        artifacts=[
            {"key": "iberia", "wkt": "POLYGON ((-10 36, 3 36, 3 44, -10 44, -10 36))",
             "space": "wgs84"},
            {"key": "box", "bbox": [0.0, 0.0, 1.0, 1.0]},
        ],
    )
    assert [row["key"] for row in block["artifacts"]] == ["iberia", "box"]
    assert block["artifacts"][0]["space"] == "wgs84"
    assert block["artifacts"][1]["bbox"] == [0.0, 0.0, 1.0, 1.0]
    assert "[[layer.artifacts]]" in db.declaration


def test_an_artifacts_frame_carries_the_artifact_tables_columns(db):
    block = db.declare_layer(
        "cases",
        kind="flat",
        artifacts=pd.DataFrame(
            {"key": ["a", "b"], "level": [0, 0], "excluding": [[1], [2]],
             "contents": [[["Ward A"]], [["Ward B"]]]}
        ),
    )
    assert [row["key"] for row in block["artifacts"]] == ["a", "b"]
    assert block["artifacts"][0]["excluding"] == [1]
    assert block["value_set"] == "closed"


def test_an_artifact_hangs_from_the_edge_attached_to_names(db):
    db.declare_layer("clusters", kind="flat", members="members")
    block = db.declare_layer(
        "topics",
        kind="flat",
        depends_on=["clusters"],
        artifacts=[{"key": "t0", "members": [1],
                    "attached_to": {"layer": "clusters", "key": "a", "level": 0}}],
    )
    row = block["artifacts"][0]
    assert row["attached_layer"] == "clusters" and row["attached_key"] == "a"
    assert row["attached_level"] == 0
    assert block["depends_on"] == ["clusters"]


# ---------------------------------------------------------------------------- the other keys


def test_a_level_carries_its_own_zoom_range_and_a_layer_prunes_its_children(db):
    block = db.declare_layer(
        "taxonomy",
        kind="tiered",
        members="members",
        levels=[(0, "Family", [0, 5]), (1, "Genus", [4, 10])],
        prune_children=True,
    )
    assert block["levels"] == [
        {"level": 0, "title": "Family", "zoom": [0, 5]},
        {"level": 1, "title": "Genus", "zoom": [4, 10]},
    ]
    assert block["hierarchy"]["prune_children"] is True


def test_the_serving_layout_is_pinned_by_a_word(db):
    block = db.declare_layer("clusters", kind="flat", members="members", layout="column")
    assert block["layout"] == "column"


def test_artifact_visibility_is_a_label_inherited_or_a_column_and_its_default(db):
    one = db.declare_layer("a", kind="flat", members="members", artifact_visibility="ir:analyst")
    two = db.declare_layer("b", kind="flat", members="members")
    three = db.declare_layer("c", kind="flat", members="members",
                             artifact_visibility={"field": "access", "default": "inherited"})
    assert one["artifact_visibility"] == Inline({"default": "ir:analyst"})
    assert two["artifact_visibility"] == Inline({"default": "inherited"})
    assert three["artifact_visibility"] == Inline({"field": "access", "default": "inherited"})


# ---------------------------------------------------------------------------- vocabularies


def test_a_vocabulary_carries_its_values_inline_as_keys_or_as_pinned_codes(db):
    keys = db.declare_vocabulary("mood", closed=True, width="u8", values=["calm", "wild"])
    codes = db.declare_vocabulary("kind", closed=True, width="u8", values={"road": 1, "rail": 2},
                                  reserved=[7, 9])
    assert keys["values"] == ["calm", "wild"]
    assert codes["values"] == Inline({"road": 1, "rail": 2})
    assert codes["reserved"] == [7, 9]
    text = db.declaration
    assert 'values = ["calm", "wild"]' in text
    assert "values = { road = 1, rail = 2 }" in text
    assert "reserved = [7, 9]" in text


def test_a_derived_vocabulary_and_an_open_one_naming_a_source_for_titles(db):
    db.stage("titles", pd.DataFrame({"key": ["a"], "title": ["A"]}))
    derived = db.declare_vocabulary("country", closed=True, width="u16", visibility="derived",
                                    values=["fr"])
    open_set = db.declare_vocabulary("type", source="titles", width="u8",
                                     fields={"key": "key", "title": "title"})
    assert derived["visibility"] == "derived"
    assert open_set["value_set"] == "open" and open_set["source"] == "titles"
    assert open_set["fields"] == Inline({"key": "key", "title": "title"})


def test_a_vocabulary_gate_is_one_axis_with_two_settings(db):
    with pytest.raises(Refusal, match="one axis, two settings"):
        db.declare_vocabulary("country", closed=True, values=["fr"], visibility="ir:analyst")


# ---------------------------------------------------------------------------- the refusals


def test_a_shape_on_a_layer_that_is_not_spatial_is_refused_naming_the_remedy(db):
    with pytest.raises(Refusal, match='Give membership="spatial", or drop shape='):
        db.declare_layer("clusters", kind="flat", members="members", shape={"kind": "polygon"})
    with pytest.raises(Refusal, match="default_space="):
        db.declare_layer("clusters", kind="flat", members="members", default_space="wgs84")


def test_a_shape_on_an_artifact_row_of_a_layer_that_evaluates_none_is_refused(db):
    with pytest.raises(Refusal, match='Give membership="spatial"'):
        db.declare_layer("cases", kind="flat", artifacts=[{"key": "a", "wkt": "POLYGON EMPTY"}])


def test_excluding_beside_members_on_one_row_is_refused(db):
    with pytest.raises(Refusal, match="Drop members= or excluding="):
        db.declare_layer("cases", kind="flat",
                         artifacts=[{"key": "a", "members": [1], "excluding": [2]}])


def test_levels_on_a_nested_layer_are_refused_naming_the_remedy(db):
    with pytest.raises(Refusal, match="declare the layer as tiered"):
        db.declare_layer("clusters", kind="nested", members="members", levels=[(0, "Top", None)])


def test_a_spatial_layer_declares_no_stored_member_set(db):
    with pytest.raises(Refusal, match="no stored member set"):
        db.declare_layer("regions", kind="flat", membership="spatial", shape="bbox",
                         members="members")


def test_a_spatial_layer_with_neither_a_table_nor_inline_rows_is_refused(db):
    with pytest.raises(Refusal, match="Give source= or artifacts="):
        db.declare_layer("regions", kind="flat", membership="spatial", shape="bbox")


def test_inline_artifacts_beside_a_source_are_refused(db):
    with pytest.raises(Refusal, match="artifacts= is the roster"):
        db.declare_layer("cases", kind="flat", source="shapes",
                         artifacts=[{"key": "a", "members": [1]}])


def test_an_inline_artifact_is_spelled_with_the_artifact_tables_own_keys(db):
    with pytest.raises(Refusal, match="names nothing"):
        db.declare_layer("cases", kind="flat", artifacts=[{"key": "a", "colour": "red"}])
    with pytest.raises(Refusal, match="names itself"):
        db.declare_layer("cases", kind="flat", artifacts=[{"members": [1]}])


def test_one_row_carries_one_shape(db):
    with pytest.raises(Refusal, match="is two shapes"):
        db.declare_layer("regions", kind="flat", membership="spatial", shape="bbox",
                         artifacts=[{"key": "a", "bbox": [0, 0, 1, 1], "circle": [0, 0, 1]}])


def test_a_shape_is_written_in_the_view_space_or_in_wgs84(db):
    with pytest.raises(Refusal, match="'view' or 'wgs84'"):
        db.declare_layer("regions", kind="flat", membership="spatial", shape="bbox",
                         artifacts=[{"key": "a", "bbox": [0, 0, 1, 1], "space": "utm"}])


def test_an_attribute_membership_refuses_what_it_would_not_serve(db):
    for kwargs, why in [
        ({"kind": "tiered", "levels": [(0, "Top", None)]}, 'kind is "flat"'),
        ({"supplied": [("topic", "text", "all")]}, "supplied="),
        ({"depends_on": ["other"]}, "depends_on="),
        ({"layout": "column"}, "layout="),
        ({"members": "members"}, "members="),
        ({"artifact_visibility": {"field": "access", "default": "inherited"}}, "no column"),
    ]:
        with pytest.raises(Refusal, match=why):
            db.declare_layer("programmes", kind=kwargs.pop("kind", "flat"),
                             membership={"attribute": "kind"}, **kwargs)


def test_a_scoped_layer_names_the_column_its_rows_carry_the_view_in(db):
    with pytest.raises(Refusal, match="fields=\\{'view': column\\}"):
        db.declare_layer("q", kind="flat", source="members", scope={"group": "quarter"})


def test_the_layout_pin_is_one_of_three_words(db):
    with pytest.raises(Refusal, match="serving-layout pin"):
        db.declare_layer("clusters", kind="flat", members="members", layout="rowmajor")


def test_a_membership_is_two_words_or_one_table(db):
    with pytest.raises(Refusal, match="'enumerated', 'spatial'"):
        db.declare_layer("clusters", kind="flat", members="members", membership="derived")
    with pytest.raises(Refusal, match="names nothing else"):
        db.declare_layer("clusters", kind="flat", membership={"attribute": "k", "depth": 3})


def test_withdrawing_a_whole_artifact_is_refused_as_unbuilt(db):
    with pytest.raises(Refusal, match="specified and not built"):
        db.declare_layer("cases", kind="flat", members="members",
                         withdraw_on_member_deletion=True)
