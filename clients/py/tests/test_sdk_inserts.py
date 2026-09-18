"""Inserting, identity and the directory: python-sdk.md §2, §3 and §4.

`declare_*` says what exists; `insert` hands it a table and names the columns it reads. Nothing
here is matched by name except an attribute's value column on the allocation view's own insert,
which `test_sdk_columns.py` covers.
"""

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from tesseradb._database import Database, create
from tesseradb._refusal import Refusal

pd = pytest.importorskip("pandas")


def frame(n=3, **columns):
    return pd.DataFrame({"x": [float(i) for i in range(n)], "y": [0.0] * n, **columns})


def mapped(tmp_path, name="db"):
    db = create(tmp_path / name)
    db.declare_view("map")
    return db


def test_a_frame_is_written_as_it_was_given_and_the_declaration_names_the_columns(tmp_path):
    db = mapped(tmp_path)
    insert = db.insert("map", frame(id=["p", "q", "r"]), id="id", x="x", y="y")
    table = pq.read_table(insert.path)
    # Nothing is minted and nothing is renamed: the file is the frame, and the declaration is
    # what says where identity is.
    assert table.column_names == ["x", "y", "id"]
    assert table["id"].to_pylist() == ["p", "q", "r"]
    assert insert.columns == {"id": "id", "x": "x", "y": "y"}
    assert 'entity_id = "id"' in db.declaration


def test_every_insert_prints_the_columns_it_read_and_the_columns_it_ignored(tmp_path, capsys):
    db = mapped(tmp_path)
    insert = db.insert("map", frame(id=["p", "q", "r"], note=["a", "b", "c"]), x="x", y="y")
    assert insert.read == ["x", "y"]
    assert insert.ignored == ["id", "note"]
    printed = capsys.readouterr().out
    assert "read:    x, y" in printed and "ignored: id, note" in printed


def test_a_column_the_target_does_not_read_is_refused_at_the_verb(tmp_path):
    db = mapped(tmp_path)
    with pytest.raises(Refusal, match="names nothing this target reads"):
        db.insert("map", frame(), x="x", y="y", key="x")


def test_a_column_the_target_needs_and_the_call_does_not_name_is_refused(tmp_path):
    db = mapped(tmp_path)
    with pytest.raises(Refusal, match="y= is not named"):
        db.insert("map", frame(), x="x")


def test_a_name_that_is_no_column_of_the_table_is_refused_naming_the_columns(tmp_path):
    db = mapped(tmp_path)
    with pytest.raises(Refusal, match="names no column of this table"):
        db.insert("map", frame(), x="x", y="y", id="paper")


def test_an_insert_on_something_undeclared_names_the_verb_that_declares_it(tmp_path):
    db = create(tmp_path / "db")
    with pytest.raises(Refusal, match="nothing of that name is declared"):
        db.insert("map", frame(), x="x", y="y")


def test_a_frame_with_no_id_is_the_tessera_id_route(tmp_path):
    """§3: a row position names a row only while the frame is the whole corpus.

    A frame inserted with no `id=` is the Tessera-id route: the build writes no external id and a
    row is addressable by the `tessera_id` a pick or the ingest route hands back.
    """
    db = mapped(tmp_path)
    insert = db.insert("map", frame(id=["p", "q", "r"]), x="x", y="y")
    assert insert.id_column is None
    assert "entity_id" not in db.declaration
    assert db._id_arguments() == []


def test_a_file_is_read_where_it_lies_whatever_its_id_column_holds(tmp_path):
    for column, values in (
        ("entity_id", pa.array([7, 8], pa.uint64())),
        ("paper", pa.array(["p", "q"], pa.string())),
    ):
        path = tmp_path / f"{column}.parquet"
        pq.write_table(pa.table({column: values, "x": [1.0, 2.0], "y": [0.0, 0.0]}), path)
        db = mapped(tmp_path, f"db-{column}")
        insert = db.insert("map", str(path), id=column, x="x", y="y")
        assert insert.in_place and insert.path == path
        assert insert.id_column == column
        assert not insert.declared_path.startswith("/")


def test_a_path_that_does_not_exist_is_refused(tmp_path):
    db = mapped(tmp_path)
    with pytest.raises(Refusal, match="does not exist"):
        db.insert("map", str(tmp_path / "absent.parquet"), x="x", y="y")


def test_several_inserts_on_one_target_before_a_commit_accumulate(tmp_path):
    """§3: a corpus in parts is loaded by the same calls as one file."""
    db = mapped(tmp_path)
    db.insert("map", frame(2, id=["p", "q"]), id="id", x="x", y="y")
    insert = db.insert("map", frame(3, id=["r", "s", "t"]), id="id", x="x", y="y")
    assert insert.rows == 5
    assert len([one for one in db.inserts if one.target == "map"]) == 1
    assert db.declaration.count("map = ") == 1


def test_a_second_part_naming_its_columns_differently_is_refused(tmp_path):
    db = mapped(tmp_path)
    db.insert("map", frame(2, id=["p", "q"]), id="id", x="x", y="y")
    with pytest.raises(Refusal, match="names its columns differently"):
        db.insert("map", frame(2, paper=["r", "s"]), id="paper", x="x", y="y")


def test_two_tables_in_one_call_are_refused_naming_the_two_calls(tmp_path):
    db = create(tmp_path / "db")
    db.declare_view("map")
    db.declare_layer("clusters", kind="flat")
    with pytest.raises(Refusal, match="two tables"):
        db.insert(
            "clusters",
            artifacts=pd.DataFrame({"key": ["a"]}),
            members=pd.DataFrame({"key": ["a"], "entity": ["p"]}),
            id="entity",
        )


def test_a_layers_two_tables_are_two_inserts_with_their_own_column_names(tmp_path):
    db = create(tmp_path / "db")
    db.declare_view("map")
    db.declare_layer("clusters", kind="flat")
    db.insert("map", frame(id=["p", "q", "r"]), id="id", x="x", y="y")
    db.insert(
        "clusters",
        artifacts=pd.DataFrame({"k": ["a"], "lvl": [0]}),
        key="k",
        level="lvl",
    )
    db.insert(
        "clusters",
        members=pd.DataFrame({"k": ["a"], "e": ["p"]}),
        id="e",
        key="k",
    )
    text = db.declaration
    assert 'fields = { key = "k", level = "lvl" }' in text
    assert 'fields = { key = "k", entity = "e" }' in text


def test_create_refuses_a_directory_that_is_not_empty(tmp_path):
    (tmp_path / "db").mkdir()
    (tmp_path / "db" / "something").write_text("here")
    with pytest.raises(Refusal, match="open\\(\\).*replace=True"):
        create(tmp_path / "db")
    # `replace=True` removes a Tessera database. A directory of somebody else's files is refused
    # naming it, since the alternative is deleting work nobody asked about.
    with pytest.raises(Refusal, match="holds no tessera.toml"):
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
    db.insert("map", frame(id=["p", "q", "r"]), id="id", x="x", y="y")
    db.write()
    target = db.save(tmp_path / "kept")
    assert (target / "schema.toml").exists() and (target / "tessera.toml").exists()
    db.close()
    assert (target / "schema.toml").exists()


def test_open_reads_the_blocks_and_the_inserts_back_from_the_sdks_own_copy(tmp_path):
    db = mapped(tmp_path)
    db.insert("map", frame(id=["p", "q", "r"]), id="id", x="x", y="y")
    db.declare_attribute("y", type="f64", render=True)
    db.write()
    from tesseradb._database import open as open_database

    # A database saved before its first commit reopens where it was left, and the blocks it holds
    # are the ones the verbs built rather than a re-reading of the TOML.
    again = open_database(tmp_path / "db")
    assert again.blocks.view_names() == ["map"]
    assert [one.target for one in again.inserts] == ["map"]
    assert again.inserts[0].id_column == "id"
    assert again.declaration == db.declaration
    again.declare_attribute("z", type="f64")
    assert 'name = "z"' in again.declaration
    with pytest.raises(Refusal, match="no tessera.toml"):
        open_database(tmp_path)


def test_a_built_database_takes_every_declaration_but_a_render_column(tmp_path):
    """Every block kind is declared at any commit; `render=True` is the one the route refuses.

    A rendered value is served from the hot column of the row that carries it, and
    `PUT /control/attributes` declares a column against entities that already exist, so the SDK
    refuses it at the verb naming the first commit (decision 0136's amendment, §4.5).
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
        with pytest.raises(Refusal, match="render=True is fixed at the first commit"):
            call()


def test_a_key_column_inserted_into_a_layer_after_the_first_commit_is_refused(tmp_path):
    """§6.2 step 3: the column mints at the build and on the ingest route, and nowhere else."""
    db = mapped(tmp_path)
    db.declare_layer("clusters", kind="flat")
    db.built = True
    with pytest.raises(Refusal, match="Not built yet: the values route reads a layer column"):
        db.insert("clusters", frame(id=["p", "q", "r"], cluster=["a", "a", "b"]),
                  id="id", key="cluster")


def test_a_label_set_with_no_generating_set_is_refused_naming_the_rule_it_needs(tmp_path):
    """§4.7: a label with no members of its own is served to nobody until H lands."""
    db = mapped(tmp_path)
    db.declare_layer("clusters", kind="flat")
    db.declare_labels("topics", of="clusters")
    with pytest.raises(Refusal, match="Not built yet: the engine places an attached artifact"):
        db.insert("topics", {"a": "Diffusion models"})


def test_a_second_views_insert_without_the_access_column_is_refused_naming_it(tmp_path):
    """§4.2: the labels are the entity's, and the SDK copies nothing between views."""
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
    db.insert("knn", first, id="id", x="x", y="y", access="terms")
    with pytest.raises(Refusal, match="Name this frame's own label column with access="):
        db.insert("pca", first.drop(columns=["terms"]), id="id", x="x", y="y")


def test_a_first_commit_with_no_rows_refuses_every_fitted_frame(tmp_path):
    for extent in (None, "auto", {"auto": True, "margin": 0.1}):
        db = create(tmp_path / f"db{extent!s:.6}", replace=True)
        db.declare_view("s0", extent=extent)
        db.insert("s0", pd.DataFrame({"id": [], "x": [], "y": []}), id="id", x="x", y="y")
        with pytest.raises(Refusal, match="needs extent="):
            db.commit()
    db = create(tmp_path / "stated")
    db.declare_view("s0", extent={"x": [0.0, 1.0], "y": [0.0, 1.0]})
    db.insert("s0", pd.DataFrame({"id": [], "x": [], "y": []}), id="id", x="x", y="y")
    db._refuse_an_empty_build(db._document())


def test_one_name_held_by_two_kinds_is_told_apart_by_the_columns_the_call_names(tmp_path):
    """A category column and the value set it reads are declared under one name (§4.4, §4.5).

    An attribute reads `id=` and `value=`; a vocabulary reads `key=`, `title=` and `code=`. The
    columns the call names are what say which of the two the table is for.
    """
    db = create(tmp_path / "db")
    db.declare_view("map")
    db.declare_vocabulary("venue", closed=True, width="u8")
    db.declare_attribute("venue", type="category", vocabulary="venue", index=True)
    values = db.insert(
        "venue", pd.DataFrame({"key": ["icml"], "title": ["ICML"]}), key="key", title="title"
    )
    assert values.kind == "vocabulary"
    cells = db.insert(
        "venue",
        pd.DataFrame({"id": ["p0"], "venue": ["icml"]}),
        id="id",
        value="venue",
    )
    assert cells.kind == "attribute"
    with pytest.raises(Refusal, match="are declared under that name"):
        db.insert("venue", pd.DataFrame({"key": ["icml"]}))
