"""Inserting, identity and the directory.

`declare_*` says what exists; `insert` hands it a table and names the columns it reads. Nothing
here is matched by name except a unique attribute's column, which names each row's item, and an
attribute's value column on the allocation view's own insert, which `test_sdk_columns.py` covers.
"""

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from conftest import binary
from tesseradb._database import Database, create
from tesseradb._database import open as open_database
from tesseradb._refusal import Refusal

pd = pytest.importorskip("pandas")


def frame(n=3, **columns):
    return pd.DataFrame({"x": [float(i) for i in range(n)], "y": [0.0] * n, **columns})


def joined(db, name="id", type="keyword"):
    """The frames' id column, declared unique."""
    db.declare_attribute(name, type=type, unique=True)


def mapped(tmp_path, name="db", unique="id", type="keyword"):
    db = create(tmp_path / name)
    db.declare_view("map")
    joined(db, unique, type)
    return db


def test_a_frame_is_written_as_it_was_given_and_the_declaration_names_the_columns(tmp_path):
    db = mapped(tmp_path)
    insert = db.insert("map", frame(id=["p", "q", "r"]), x="x", y="y")
    table = pq.read_table(insert.path)
    # Nothing is minted and nothing is renamed: the file is the frame, and the unique column
    # names each row's item under its own name.
    assert table.column_names == ["x", "y", "id"]
    assert table["id"].to_pylist() == ["p", "q", "r"]
    assert insert.columns == {"x": "x", "y": "y"}
    assert insert.items == {"id": "id"}
    assert insert.read == ["x", "y", "id"]


def test_every_insert_returns_the_columns_it_read_and_the_columns_it_ignored(tmp_path):
    db = create(tmp_path / "db")
    db.declare_view("map")
    insert = db.insert("map", frame(id=["p", "q", "r"], note=["a", "b", "c"]), x="x", y="y")
    assert insert.read == ["x", "y"]
    assert insert.ignored == ["id", "note"]


def test_a_column_the_target_does_not_read_is_refused_at_the_verb(tmp_path):
    db = mapped(tmp_path)
    with pytest.raises(Refusal):
        db.insert("map", frame(), x="x", y="y", key="x")


def test_a_column_the_target_needs_and_the_call_does_not_name_is_refused(tmp_path):
    db = mapped(tmp_path)
    with pytest.raises(Refusal):
        db.insert("map", frame(), x="x")


def test_a_name_that_is_no_column_of_the_table_is_refused_naming_the_columns(tmp_path):
    db = mapped(tmp_path)
    with pytest.raises(Refusal):
        db.insert("map", frame(), x="x", y="y", columns={"id": "paper"})


def test_an_insert_on_something_undeclared_names_the_verb_that_declares_it(tmp_path):
    db = create(tmp_path / "db")
    with pytest.raises(Refusal):
        db.insert("map", frame(), x="x", y="y")


def test_a_frame_with_no_unique_column_makes_each_row_an_item(tmp_path):
    """A frame carrying no column of a unique attribute names no item: each row is an item of its
    own, addressable by the `tessera_id` a pick or the ingest route hands back."""
    db = create(tmp_path / "db")
    db.declare_view("map")
    insert = db.insert("map", frame(id=["p", "q", "r"]), x="x", y="y")
    assert insert.items == {}
    assert "unique" not in db.declaration


def test_a_file_is_read_where_it_lies_whatever_its_id_column_holds(tmp_path):
    for column, values, type in (
        ("entity_id", pa.array([7, 8], pa.uint64()), "u64"),
        ("paper", pa.array(["p", "q"], pa.string()), "keyword"),
    ):
        path = tmp_path / f"{column}.parquet"
        pq.write_table(pa.table({column: values, "x": [1.0, 2.0], "y": [0.0, 0.0]}), path)
        db = mapped(tmp_path, f"db-{column}", column, type)
        insert = db.insert("map", str(path), x="x", y="y")
        assert insert.in_place and insert.path == path
        assert insert.items == {column: column}
        assert not insert.declared_path.startswith("/")


def test_a_path_that_does_not_exist_is_refused(tmp_path):
    db = mapped(tmp_path)
    with pytest.raises(Refusal):
        db.insert("map", str(tmp_path / "absent.parquet"), x="x", y="y")


def test_several_inserts_on_one_target_before_a_commit_accumulate(tmp_path):
    """A corpus in parts is loaded by the same calls as one file."""
    db = mapped(tmp_path)
    db.insert("map", frame(2, id=["p", "q"]), x="x", y="y")
    insert = db.insert("map", frame(3, id=["r", "s", "t"]), x="x", y="y")
    assert insert.rows == 5
    assert len([one for one in db.inserts if one.target == "map"]) == 1
    assert db.declaration.count("map = ") == 1


def test_a_second_part_naming_its_columns_differently_is_refused(tmp_path):
    db = mapped(tmp_path)
    db.insert("map", frame(2, id=["p", "q"]), x="x", y="y")
    with pytest.raises(Refusal):
        db.insert("map", frame(2, paper=["r", "s"]), columns={"id": "paper"}, x="x", y="y")


def test_a_second_part_whose_schema_differs_is_refused_naming_the_two_types(tmp_path):
    """The parts are written as one file, so a promoted column would be a type neither
    part was written in."""
    db = mapped(tmp_path)
    db.insert("map", frame(2, id=["p", "q"]), x="x", y="y")
    with pytest.raises(Refusal):
        db.insert("map", frame(2, id=[1, 2]), x="x", y="y")
    with pytest.raises(Refusal):
        db.insert("map", frame(2, id=["r", "s"], extra=[1, 2]), x="x", y="y")


def test_two_tables_in_one_call_are_refused_naming_the_two_calls(tmp_path):
    db = create(tmp_path / "db")
    db.declare_view("map")
    db.declare_layer("clusters", kind="flat")
    with pytest.raises(Refusal):
        db.insert(
            "clusters",
            artifacts=pd.DataFrame({"key": ["a"]}),
            members=pd.DataFrame({"key": ["a"], "entity": ["p"]}),
            columns={"id": "entity"},
        )


def test_a_layers_two_tables_are_two_inserts_with_their_own_column_names(tmp_path):
    db = mapped(tmp_path)
    db.declare_layer("clusters", kind="flat")
    db.insert("map", frame(id=["p", "q", "r"]), x="x", y="y")
    db.insert("clusters", artifacts=pd.DataFrame({"k": ["a"]}), key="k")
    db.insert(
        "clusters", members=pd.DataFrame({"k": ["a"], "e": ["p"]}), key="k", columns={"id": "e"}
    )
    text = db.declaration
    assert 'fields = { key = "k" }' in text
    assert 'fields = { key = "k", id = "e" }' in text


def test_a_column_the_build_reads_under_its_own_name_takes_no_other(tmp_path):
    """`level` and `attached_level` are in no `fields` map, so the table is what renames."""
    db = create(tmp_path / "db")
    db.declare_layer("clusters", kind="tiered", levels=[(0, "a"), (1, "b")])
    with pytest.raises(Refusal):
        db.insert("clusters", artifacts=pd.DataFrame({"k": ["a"], "lvl": [0]}),
                  key="k", level="lvl")
    insert = db.insert(
        "clusters", artifacts=pd.DataFrame({"k": ["a"], "level": [0]}), key="k", level="level"
    )
    assert insert.read == ["k", "level"]


def test_a_canonical_column_the_call_did_not_name_is_refused_with_its_two_remedies(tmp_path):
    """A table in Tessera's own shape is no exception to the rule that names are named."""
    db = create(tmp_path / "db")
    db.declare_layer("clusters", kind="flat")
    table = pd.DataFrame({"k": ["a"], "parent": ["b"]})
    with pytest.raises(Refusal):
        db.insert("clusters", artifacts=table, key="k")
    insert = db.insert("clusters", artifacts=table, key="k", parent="parent")
    assert insert.read == ["k", "parent"] and insert.ignored == []


def test_a_shape_is_named_by_its_kind_and_read_under_its_own_columns(tmp_path):
    db = create(tmp_path / "db")
    db.declare_layer("regions", kind="flat", membership="spatial", shape="bbox")
    table = pd.DataFrame(
        {"k": ["a"], "min_x": [0.0], "min_y": [0.0], "max_x": [1.0], "max_y": [1.0]}
    )
    with pytest.raises(Refusal):
        db.insert("regions", artifacts=table, key="k")
    with pytest.raises(Refusal):
        db.insert("regions", artifacts=table, key="k", shape="hexagon")
    with pytest.raises(Refusal):
        db.insert("regions", artifacts=table, key="k", shape="polygon")
    insert = db.insert("regions", artifacts=table, key="k", shape="bbox")
    assert insert.shape == "bbox" and insert.ignored == []


def test_create_refuses_a_directory_that_is_not_empty(tmp_path):
    (tmp_path / "db").mkdir()
    (tmp_path / "db" / "something").write_text("here")
    with pytest.raises(Refusal):
        create(tmp_path / "db")
    # `replace=True` removes a Tessera database. A directory of somebody else's files is refused
    # naming it, since the alternative is deleting work nobody asked about.
    with pytest.raises(Refusal):
        create(tmp_path / "db", replace=True)
    (tmp_path / "db" / "tessera.toml").write_text("")
    database = create(tmp_path / "db", replace=True)
    assert not (database.path / "something").exists()


def test_a_temporary_database_goes_on_a_ram_backed_filesystem_and_close_removes_it():
    db = create()
    assert db.temporary
    path = db.path
    assert path.exists()
    db.close()
    assert not path.exists()


def test_save_copies_a_temporary_database_out(tmp_path):
    db = create()
    db.declare_view("map")
    joined(db)
    db.insert("map", frame(id=["p", "q", "r"]), x="x", y="y")
    db.write()
    target = db.save(tmp_path / "kept")
    assert (target / "schema.toml").exists() and (target / "tessera.toml").exists()
    db.close()
    assert (target / "schema.toml").exists()


def test_open_reads_the_blocks_and_the_inserts_back_from_the_sdks_own_copy(tmp_path):
    db = mapped(tmp_path)
    db.insert("map", frame(id=["p", "q", "r"]), x="x", y="y")
    db.declare_attribute("y", type="f64", render=True)
    db.write()
    from tesseradb._database import open as open_database

    # A database saved before its first commit reopens where it was left, and the blocks it holds
    # are the ones the verbs built rather than a re-reading of the TOML.
    again = open_database(tmp_path / "db")
    assert again.blocks.view_names() == ["map"]
    assert [one.target for one in again.inserts] == ["map"]
    assert again.inserts[0].items == {"id": "id"}
    assert again.declaration == db.declaration
    again.declare_attribute("z", type="f64")
    assert 'name = "z"' in again.declaration
    with pytest.raises(Refusal):
        open_database(tmp_path)


def test_a_built_database_takes_every_declaration_but_a_render_column(tmp_path):
    """Every block kind is declared at any commit; `render=True` is the one the route refuses.

    A rendered value is served from the hot column of the row that carries it, and
    `PUT /control/attributes` declares a column against entities that already exist, so the SDK
    refuses it at the verb, naming the first commit.
    """
    db = Database(tmp_path)
    db.built = True
    assert db.declare_attribute("a", type="u8")["name"] == "a"
    assert db.declare_vocabulary("kinds", values=["a"], closed=True)["name"] == "kinds"
    assert db.declare("attribute", {"name": "b", "type": "u8"})["name"] == "b"
    assert db.declare("layer", {"name": "l"})["name"] == "l"
    assert db.declare_view("v")["name"] == "v"
    assert db.declare_view_group("g")["name"] == "g"
    for call in (
        lambda: db.declare_attribute("hot", type="u8", render=True),
        lambda: db.declare("attribute", {"name": "hot", "type": "u8", "render": True}),
    ):
        with pytest.raises(Refusal):
            call()


def test_a_key_column_is_inserted_into_a_layer_at_any_commit(tmp_path):
    """The values route mints and joins from a key column as the other doors do."""
    db = mapped(tmp_path)
    db.declare_layer("clusters", kind="flat")
    db.built = True
    insert = db.insert(
        "clusters", frame(id=["p", "q", "r"], cluster=["a", "a", "b"]), key="cluster"
    )
    assert insert.columns == {"key": "cluster"} and insert.items == {"id": "id"}
    assert [one.target for one in db.pending] == ["clusters"]


def test_a_label_set_takes_its_text_and_needs_no_members_of_its_own(tmp_path):
    """Such a label is the label of its cluster, and the engine places it."""
    db = mapped(tmp_path)
    db.declare_layer("clusters", kind="flat")
    db.declare_labels("topics", of="clusters")
    db.insert("map", frame(id=["p", "q", "r"]), x="x", y="y")
    db.insert("clusters", members=pd.DataFrame({"key": ["a"], "entity": ["p"]}),
              columns={"id": "entity"}, key="key")
    db.insert("topics", {"a": "Diffusion models"})
    assert db._preflight(db._document()) == []


def test_a_second_views_insert_without_the_access_column_is_refused_naming_it(tmp_path):
    """The labels are the entity's, and the SDK copies nothing between views."""
    db = create(tmp_path / "db")
    first = pd.DataFrame(
        {
            "id": ["p", "q", "r"],
            "x": [0.0, 1.0, 2.0],
            "y": [0.0, 0.0, 0.0],
            "terms": [["a"], ["b"], ["a"]],
        }
    )
    db.declare_view("knn")
    db.declare_view("pca")
    joined(db)
    db.insert("knn", first, x="x", y="y", access="terms")
    with pytest.raises(Refusal):
        db.insert("pca", first.drop(columns=["terms"]), x="x", y="y")


def test_a_first_commit_with_no_rows_refuses_every_fitted_frame(tmp_path):
    for extent in (None, "auto", {"auto": True, "margin": 0.1}):
        db = create(tmp_path / f"db{extent!s:.6}", replace=True)
        db.declare_view("s0", extent=extent)
        joined(db)
        db.insert("s0", pd.DataFrame({"id": [], "x": [], "y": []}), x="x", y="y")
        with pytest.raises(Refusal):
            db.commit()
    db = create(tmp_path / "stated")
    db.declare_view("s0", extent={"x": [0.0, 1.0], "y": [0.0, 1.0]})
    joined(db)
    db.insert("s0", pd.DataFrame({"id": [], "x": [], "y": []}), x="x", y="y")
    db._refuse_an_empty_build(db._document())


def test_one_name_held_by_two_kinds_is_told_apart_by_the_columns_the_call_names(tmp_path):
    """A category column and the value set it reads are declared under one name.

    An attribute reads `value=`; a vocabulary reads `key=`, `title=` and `code=`. The
    columns the call names are what say which of the two the table is for.
    """
    db = mapped(tmp_path)
    db.declare_vocabulary("venue", closed=True, width="u8")
    db.declare_attribute("venue", type="category", vocabulary="venue", index=True)
    values = db.insert(
        "venue", pd.DataFrame({"key": ["icml"], "title": ["ICML"]}), key="key", title="title"
    )
    assert values.kind == "vocabulary"
    cells = db.insert(
        "venue",
        pd.DataFrame({"id": ["p0"], "venue": ["icml"]}),
        value="venue",
    )
    assert cells.kind == "attribute"
    with pytest.raises(Refusal):
        db.insert("venue", pd.DataFrame({"key": ["icml"]}))


def test_a_committed_database_reopens_and_takes_the_next_commit(tmp_path):
    """Everything a commit wrote is in the directory: the bundle, the blocks and the ids.

    A database committed, closed and opened again is a built database, so the next insert pages
    through the control plane rather than building a second time, and what the first commit wrote
    is still there to be read.
    """
    binary()
    db = create(tmp_path / "db")
    db.declare_view("map", extent={"min": 0.0, "max": 8.0})
    joined(db, type="i64")
    db.insert("map", frame(id=[1, 2, 3], access=["public"] * 3),
              x="x", y="y", access="access")
    first = db.commit()
    assert first.ok, first.log
    db.close()

    again = open_database(tmp_path / "db")
    try:
        assert again.built
        assert again.blocks.view_names() == ["map"]
        assert [view["id"] for view in again.meta()["views"]] == ["map"]
        again.insert("map", frame(n=2, id=[4, 5], access=["public"] * 2),
                     x="x", y="y", access="access")
        second = again.commit()
        assert second.ok, second
        assert second.sent and second.rows == 2
    finally:
        again.close()



def test_a_padded_label_serves_trimmed_and_a_conjunction_needs_both_terms_at_each_commit(served):
    """The server reads the labels the SDK sends: surrounding spaces are trimmed, an empty label is
    no label, and a label holding a conjunction is satisfied by a reader holding both of its terms,
    on the build and on a later commit alike. The default reader holds every term the labels
    name."""

    def labelled(first: int) -> pa.Table:
        ids = [f"p{i}" for i in range(first, first + 3)]
        return pa.table(
            {
                "id": ids,
                "x": [float(i) for i in range(first, first + 3)],
                "y": [1.0] * 3,
                "access": [[" red ", ""], ["red&blue"], ["  "]],
            }
        )

    def first_commit(db):
        db.declare_view("map", extent={"x": [-5, 40], "y": [-5, 40]}, default_label=" sealed ")
        joined(db)
        db.insert("map", labelled(0), x="x", y="y", access="access")

    db = served(first_commit)

    def counts():
        readers = {"red": ["red"], "both": ["red", "blue"], "sealed": ["sealed"]}
        return {name: db.viewer(terms).view("map").count() for name, terms in readers.items()}

    assert counts() == {"red": 1, "both": 2, "sealed": 1}

    db.insert("map", labelled(10), x="x", y="y", access="access")
    report = db.commit()
    assert report.ok, report
    assert counts() == {"red": 2, "both": 4, "sealed": 2}
    assert {"red", "blue", "sealed"} <= set(db.terms)
    assert not {" red ", "", "  ", "red&blue"} & set(db.terms)
