"""The layer and vocabulary surface: what the verbs write, and what they refuse.

A `declare_*` verb takes no data, so a layer's tables are its inserts' and the verb writes a
declaration. What is asserted here is that declaration through the check every build runs: a
shape, an inline roster, levels and a value set reach it as the verbs wrote them. The rules the
check itself owns are its own to state, and the refusals below stand for the SDK's blocks
reaching it at all — an inline artifact row among them, which the check reads inside.

The refusals that are the SDK's own are here too: how a shape, a membership and a scope are
spelled on a Python call, which no file expresses.
"""

import pytest

from tesseradb._database import create
from tesseradb._refusal import Refusal

pd = pytest.importorskip("pandas")

WORLD = {"lon": [-180.0, 180.0], "lat": [-85.0511287798066, 85.0511287798066]}


def members():
    return pd.DataFrame({"key": ["a"], "entity": ["p0"]})


@pytest.fixture
def db(tmp_path):
    database = create(tmp_path / "db")
    database.declare_view("s0")
    return database


# ---------------------------------------------------------------------------- what is declared


def test_a_spatial_layer_names_its_shape_kind_and_the_space_its_shapes_are_written_in(
    tmp_path, checked
):
    db = create(tmp_path / "db")
    db.declare_view("world", projection="web_mercator", extent=WORLD)
    db.declare_layer(
        "regions",
        kind="flat",
        membership="spatial",
        shape={"kind": "polygon"},
        default_space="wgs84",
    )
    db.insert(
        "regions",
        artifacts=pd.DataFrame(
            {"key": ["a"], "geometry": ["POLYGON ((0 0, 1 0, 1 1, 0 0))"]}
        ),
        key="key",
        shape="polygon",
    )
    assert checked(db).ok


def test_a_shape_kind_may_be_named_bare(db, checked):
    db.declare_layer("regions", kind="flat", membership="spatial", shape="bbox")
    db.insert(
        "regions",
        artifacts=pd.DataFrame(
            {"key": ["a"], "min_x": [0.0], "min_y": [0.0], "max_x": [1.0], "max_y": [1.0]}
        ),
        key="key",
        shape="bbox",
    )
    assert checked(db).ok


def test_an_attribute_membership_is_a_predicate_over_a_declared_column(db, checked):
    db.declare_vocabulary("kind", closed=True, width="u8", values=["road", "rail"])
    db.declare_attribute("kind", type="category", vocabulary="kind", index=True)
    db.declare_layer("programmes", kind="flat", membership={"attribute": "kind"})
    assert checked(db).ok


def test_a_layer_scoped_to_a_view_group_keys_its_artifacts_per_view(tmp_path, checked):
    db = create(tmp_path / "db")
    db.declare_view_group("quarter", extent={"x": [0.0, 1.0], "y": [0.0, 1.0]})
    db.declare_layer(
        "quarter_clusters", kind="flat", views=["quarter"], scope={"group": "quarter"}
    )
    db.insert(
        "quarter",
        pd.DataFrame({"id": ["p0"], "x": [0.0], "y": [0.0], "q": ["q1"]}),
        id="id",
        x="x",
        y="y",
        view="q",
    )
    db.insert(
        "quarter_clusters",
        artifacts=pd.DataFrame({"key": ["a"], "quarter": ["q1"]}),
        key="key",
        view="quarter",
    )
    assert checked(db).ok


def test_inline_artifacts_carry_the_shape_the_space_and_the_exclusion(tmp_path, checked):
    db = create(tmp_path / "db")
    db.declare_view("world", projection="web_mercator", extent=WORLD)
    db.declare_layer(
        "regions",
        kind="flat",
        membership="spatial",
        shape={"kind": "polygon"},
        artifacts=[
            {"key": "iberia", "wkt": "POLYGON ((-10 36, 3 36, 3 44, -10 44, -10 36))",
             "space": "wgs84"},
            {"key": "japan", "wkt": "POLYGON ((129 31, 146 31, 146 46, 129 46, 129 31))",
             "space": "wgs84"},
        ],
    )
    assert checked(db).ok


def test_an_artifacts_frame_written_in_the_declaration_carries_the_tables_columns(db, checked):
    db.declare_layer(
        "cases",
        kind="flat",
        artifacts=pd.DataFrame(
            {"key": ["a", "b"], "level": [0, 0], "excluding": [[0], [1]],
             "contents": [[["Ward A"]], [["Ward B"]]]}
        ),
        supplied=[("ward", "text", "inherited")],
    )
    assert checked(db).ok


def test_an_artifact_hangs_from_the_edge_attached_to_names(db, checked):
    """`attached_to` is the call's spelling; the declaration carries the layer and the key."""
    db.declare_layer("clusters", kind="flat", artifacts=[{"key": "a", "members": [0]}])
    db.declare_layer(
        "topics",
        kind="flat",
        depends_on=["clusters"],
        artifacts=[{"key": "t0", "members": [0],
                    "attached_to": {"layer": "clusters", "key": "a", "level": 0}}],
    )
    assert checked(db).ok
    assert "attached_layer" in db.declaration and "attached_to" not in db.declaration


def test_a_level_carries_its_own_zoom_range_and_a_layer_prunes_its_children(db, checked):
    db.declare_layer(
        "taxonomy",
        kind="tiered",
        levels=[(0, "Family", [0, 5]), (1, "Genus", [4, 10])],
        prune_children=True,
    )
    db.insert("taxonomy", members=members(), id="entity", key="key")
    assert checked(db).ok


def test_the_serving_layout_and_the_artifact_gate_reach_the_declaration(db, checked):
    db.declare_layer("a", kind="flat", layout="column", artifact_visibility="ir:analyst")
    db.declare_layer("b", kind="flat")
    db.declare_layer("c", kind="flat")
    db.insert(
        "c",
        artifacts=pd.DataFrame({"key": ["a"], "team": ["public"]}),
        key="key",
        access="team",
    )
    import tomllib

    layer = next(one for one in tomllib.loads(db.declaration)["layer"] if one["name"] == "c")
    assert layer["artifact_visibility"] == {"default": "inherited", "field": "team"}
    assert checked(db).ok


def test_the_label_column_is_named_on_the_insert_and_not_on_the_layer(db):
    with pytest.raises(Refusal):
        db.declare_layer("d", kind="flat", artifact_visibility={"field": "team", "default": "inherited"})
    db.declare_layer("e", kind="flat")
    db.insert("e", artifacts=pd.DataFrame({"key": ["a"], "team": ["x"]}), key="key", access="team")
    with pytest.raises(Refusal):
        db.insert(
            "e", artifacts=pd.DataFrame({"key": ["b"], "squad": ["x"]}), key="key", access="squad"
        )


# ---------------------------------------------------------------------------- vocabularies


def test_a_vocabulary_carries_its_values_inline_as_keys_or_as_pinned_codes(db, checked):
    db.declare_vocabulary("mood", closed=True, width="u8", values=["calm", "wild"])
    db.declare_vocabulary("kind", closed=True, width="u8", values={"road": 1, "rail": 2},
                          reserved=[7, 9])
    db.declare_attribute("mood", type="category", vocabulary="mood")
    db.declare_attribute("kind", type="category", vocabulary="kind")
    assert checked(db).ok


def test_a_derived_vocabulary_and_an_open_one_taking_an_insert_for_its_titles(db, checked):
    db.declare_vocabulary("country", closed=True, width="u16", visibility="derived",
                          values=["fr"])
    db.declare_vocabulary("type", width="u8")
    db.declare_attribute("country", type="category", vocabulary="country")
    db.declare_attribute("type", type="category", vocabulary="type")
    insert = db.insert("type", pd.DataFrame({"k": ["a"], "t": ["A"]}), key="k", title="t")
    assert insert.read == ["k", "t"]
    assert checked(db).ok


# ---------------------------------------------------------------------------- the refusals


def test_the_check_refuses_a_declaration_the_verbs_wrote_and_names_the_layer(db, checked):
    """The blocks reach the check: a rule about what may be declared is the check's to state."""
    db.declare_layer("clusters", kind="nested", levels=[(0, "Top", None)])
    report = checked(db)
    assert not report.ok
    assert "clusters" in report.output


def test_a_shape_on_an_artifact_row_of_a_layer_that_evaluates_none_is_refused(db, checked):
    db.declare_layer("cases", kind="flat", artifacts=[{"key": "ring", "wkt": "POLYGON EMPTY"}])
    report = checked(db)
    assert not report.ok
    assert "cases" in report.output and "ring" in report.output


def test_one_row_carries_one_shape(db, checked):
    db.declare_layer("regions", kind="flat", membership="spatial", shape="bbox",
                     artifacts=[{"key": "both", "bbox": [0, 0, 1, 1], "circle": [0, 0, 1]}])
    report = checked(db)
    assert not report.ok
    assert "regions" in report.output and "both" in report.output


def test_a_rows_shape_is_its_layers_kind_and_no_other(db, checked):
    db.declare_layer("regions", kind="flat", membership="spatial", shape="bbox",
                     artifacts=[{"key": "round", "circle": [0, 0, 1]}])
    report = checked(db)
    assert not report.ok
    assert "regions" in report.output and "round" in report.output


def test_an_inline_roster_with_no_row_is_refused(db):
    with pytest.raises(Refusal):
        db.declare_layer("cases", kind="flat", artifacts=[])


def test_a_scoped_layers_insert_names_the_column_its_rows_carry_the_view_in(db):
    db.declare_view_group("quarter")
    db.declare_layer("q", kind="flat", views=["quarter"], scope={"group": "quarter"})
    with pytest.raises(Refusal):
        db.insert("q", members=members(), id="entity", key="key")


def test_a_scoped_block_names_a_group_that_is_declared(db):
    with pytest.raises(Refusal):
        db.declare_layer("q", kind="flat", scope={"group": "quarter"})


def test_a_membership_is_two_words_or_one_table(db):
    with pytest.raises(Refusal):
        db.declare_layer("clusters", kind="flat", membership="derived")
    with pytest.raises(Refusal):
        db.declare_layer("clusters", kind="flat", membership={"attribute": "k", "depth": 3})


def test_a_shape_declares_its_kind_and_nothing_else(db):
    with pytest.raises(Refusal):
        db.declare_layer("regions", kind="flat", membership="spatial",
                         shape={"kind": "bbox", "space": "wgs84"})
