"""Staging, identity and the directory: python-sdk.md §2 and §3."""

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from tesseradb._database import Database, create
from tesseradb._refusal import Refusal

pd = pytest.importorskip("pandas")


def frame(n=3, **columns):
    return pd.DataFrame({"x": [float(i) for i in range(n)], "y": [0.0] * n, **columns})


def test_a_frame_is_written_as_it_was_staged_and_the_id_column_names_its_rows(tmp_path):
    db = create(tmp_path / "db")
    staged = db.stage("points", frame(id=["p", "q", "r"]), id="id", default=True)
    table = pq.read_table(staged.path)
    # Nothing is minted and nothing is renamed: the file is the frame, and the declaration is
    # what says where identity is.
    assert table.column_names == ["x", "y", "id"]
    assert table["id"].to_pylist() == ["p", "q", "r"]
    assert staged.id_column == "id"


def test_a_named_index_is_the_id_column_under_its_own_name(tmp_path):
    db = create(tmp_path / "db")
    data = frame().set_index(pd.Index(["p", "q", "r"], name="paper"))
    staged = db.stage("points", data, default=True)
    assert staged.id_column == "paper"
    assert pq.read_table(staged.path)["paper"].to_pylist() == ["p", "q", "r"]


def test_an_unnamed_default_index_is_no_id_at_all(tmp_path):
    """§3: a row position names a row only while the frame is the whole corpus.

    A frame staged with nothing naming its rows is the Tessera-id route: the build writes no
    external id and a row is addressable by the `tessera_id` a pick or the ingest route hands back.
    """
    db = create(tmp_path / "db")
    staged = db.stage("points", frame(), default=True)
    assert staged.id_column is None
    assert "tessera_id" in " ".join(staged.notes)
    assert pq.read_table(staged.path).column_names == ["x", "y"]


def test_a_column_named_id_is_the_id_column_without_being_named(tmp_path):
    db = create(tmp_path / "db")
    assert db.stage("points", frame(id=["p", "q", "r"]), default=True).id_column == "id"


def test_a_file_is_read_where_it_lies_whatever_its_id_column_holds(tmp_path):
    for column, values in (
        ("entity_id", pa.array([7, 8], pa.uint64())),
        ("paper", pa.array(["p", "q"], pa.string())),
    ):
        path = tmp_path / f"{column}.parquet"
        pq.write_table(pa.table({column: values, "x": [1.0, 2.0]}), path)
        db = create(tmp_path / f"db-{column}", replace=True)
        # `entity_id` is the name configuration.md reads identity under, so it needs no id=; any
        # other column is named.
        named = None if column == "entity_id" else column
        staged = db.stage("points", str(path), id=named, default=True)
        assert staged.in_place and staged.path == path
        assert staged.id_column == column
        assert not staged.declared_path.startswith("/")


def test_a_source_naming_no_row_is_left_alone(tmp_path):
    path = tmp_path / "values.parquet"
    pq.write_table(pa.table({"key": ["low"], "code": pa.array([1], type=pa.uint32())}), path)
    db = create(tmp_path / "db")
    staged = db.stage("severity", str(path))
    assert staged.in_place and staged.id_column is None


def test_an_id_that_names_no_column_is_refused_naming_the_columns(tmp_path):
    db = create(tmp_path / "db")
    with pytest.raises(Refusal, match="names no column"):
        db.stage("points", frame(), id="paper", default=True)


def test_staging_a_name_twice_replaces_the_earlier_data(tmp_path):
    db = create(tmp_path / "db")
    db.stage("points", frame(2, id=["p", "q"]), id="id", default=True)
    db.stage("points", frame(3, id=["p", "q", "r"]), id="id", default=True)
    assert db.sources["points"].rows == 3


def test_a_second_default_replaces_the_first_and_says_so(tmp_path):
    db = create(tmp_path / "db")
    db.stage("a", frame(id=["p", "q", "r"]), id="id", default=True)
    second = db.stage("b", frame(id=["p", "q", "r"]), id="id", default=True)
    assert db.default_source == "b"
    assert "replacing 'a'" in " ".join(second.notes)


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
    db.stage("points", frame(id=["p", "q", "r"]), id="id", default=True)
    db.declare_view("map", source="points")
    db.write()
    target = db.save(tmp_path / "kept")
    assert (target / "schema.toml").exists() and (target / "tessera.toml").exists()
    db.close()
    assert (target / "schema.toml").exists()


def test_open_reads_the_blocks_back_from_the_sdks_own_copy(tmp_path):
    db = create(tmp_path / "db")
    db.stage("points", frame(id=["p", "q", "r"]), id="id", default=True)
    db.declare_view("map", source="points")
    db.declare_attribute("y", type="f64", render=True)
    db.write()
    from tesseradb._database import open as open_database

    # A database saved before its first commit reopens where it was left, and the blocks it holds
    # are the ones the verbs built rather than a re-reading of the TOML.
    again = open_database(tmp_path / "db")
    assert again.blocks.view_names() == ["map"]
    assert again.default_source == "points"
    assert again.sources["points"].id_column == "id"
    assert again.declaration == db.declaration
    again.declare_attribute("x", type="f64")
    assert 'name = "x"' in again.declaration
    with pytest.raises(Refusal, match="no tessera.toml"):
        open_database(tmp_path)


def test_a_built_database_refuses_the_declarations_that_have_no_runtime_route(tmp_path):
    """A layer, a view and a group are sent to the running service; an attribute and a vocabulary
    are the two the SDK does not send, so they refuse on a built database and name a rebuild."""
    db = Database(tmp_path)
    db.built = True
    for call in (
        lambda: db.declare_attribute("a", type="u8"),
        lambda: db.declare_vocabulary("v", values=["a"], closed=True),
        lambda: db.declare("attribute", {"name": "a", "type": "u8"}),
    ):
        with pytest.raises(Refusal, match="not built yet"):
            call()
    # A layer, a view and a view group are declarable, the generic form included.
    assert db.declare("layer", {"name": "l"})["name"] == "l"
    assert db.declare_view("v", source="points")["name"] == "v"
    assert db.declare_view_group("g", source="points", view_field="q")["name"] == "g"


def test_a_from_column_layer_after_the_first_commit_is_refused_naming_the_tables(tmp_path):
    """§6.2 step 3: the column is read at the build and by the ingest route, and nowhere else."""
    db = create(tmp_path / "db")
    db.stage("points", frame(id=["p", "q", "r"], cluster=["a", "a", "b"]), id="id", default=True)
    db.declare_view("map", source="points")
    db.built = True
    with pytest.raises(Refusal, match="artifacts table|source=<artifacts>"):
        db.declare_layer("clusters", kind="flat", from_column="cluster")


def test_a_delta_names_a_source_the_declaration_knows(tmp_path):
    db = create(tmp_path / "db")
    db.stage("points", frame(id=["a", "b", "c"]), id="id", default=True)
    db.declare_view("map", source="points")
    db.built = True
    with pytest.raises(Refusal, match="no block of this declaration reads"):
        db.stage("nothing_reads_this", frame(id=["a", "b", "c"]), id="id")
    with pytest.raises(Refusal, match="default source"):
        db.stage("points", frame(id=["a", "b", "c"]), id="id", default=True)


def test_a_delta_carries_the_id_column_the_source_names_its_rows_by(tmp_path):
    db = create(tmp_path / "db")
    db.stage("points", frame(id=["a", "b", "c"]), id="id", default=True)
    db.declare_view("map", source="points")
    db.built = True
    with pytest.raises(Refusal, match="names its rows by 'id'"):
        db.stage("points", frame(2))


def test_a_delta_that_names_its_rows_on_a_database_that_names_none_is_refused(tmp_path):
    """The mirror of the rule above: this database's rows are tessera_id rows (§3)."""
    db = create(tmp_path / "db")
    db.stage("points", frame(), default=True)
    db.declare_view("map", source="points")
    db.built = True
    with pytest.raises(Refusal, match="names its rows by nothing"):
        db.stage("points", frame(id=["a", "b", "c"]), id="id")


def test_a_second_views_frame_without_the_access_column_is_refused_naming_it(tmp_path):
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
    db.stage("points", first, id="id", default=True)
    db.stage("points_pca", first.drop(columns=["terms"]), id="id")
    db.declare_view("knn", source="points", access="terms")
    db.declare_view("pca", source="points_pca", access="terms")
    with pytest.raises(Refusal, match="'terms'"):
        db.check()


def test_a_first_commit_with_no_rows_refuses_every_fitted_frame(tmp_path):
    for extent in (None, "auto", {"auto": True, "margin": 0.1}):
        db = create(tmp_path / f"db{extent!s:.6}", replace=True)
        db.stage("points", pd.DataFrame({"id": [], "x": [], "y": []}), id="id", default=True)
        db.declare_view("s0", extent=extent)
        with pytest.raises(Refusal, match="needs extent="):
            db.commit()
    db = create(tmp_path / "stated")
    db.stage("points", pd.DataFrame({"id": [], "x": [], "y": []}), id="id", default=True)
    db.declare_view("s0", extent={"x": [0.0, 1.0], "y": [0.0, 1.0]})
    db._refuse_an_empty_build(db._document())
