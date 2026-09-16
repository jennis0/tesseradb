"""Staging, the id map and the directory: python-sdk.md §2 and §3."""

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from tesseradb._database import Database, create
from tesseradb._idmap import ACKNOWLEDGED, ASSIGNED, REMOVED, IdMap
from tesseradb._sources import Refusal

pd = pytest.importorskip("pandas")


def frame(n=3, **columns):
    return pd.DataFrame({"x": [float(i) for i in range(n)], "y": [0.0] * n, **columns})


def test_an_id_carries_a_state_from_staging_to_the_commit(tmp_path):
    id_map = IdMap(tmp_path / "idmap.json")
    assert id_map.source_ids(["a", "b"]) == [1, 2]
    assert id_map.state_of("a") == ASSIGNED
    # A re-staged id keeps its source id: one user id is one entity everywhere.
    assert id_map.source_ids(["b", "c"]) == [2, 3]
    id_map.acknowledge(["a"])
    assert id_map.state_of("a") == ACKNOWLEDGED
    assert id_map.acknowledged() == {1}
    id_map.remove(["b"])
    assert id_map.state_of("b") == REMOVED
    id_map.save()
    assert IdMap(tmp_path / "idmap.json").state_of("b") == REMOVED
    # The sequence continues across commits rather than restarting at the next staging.
    assert IdMap(tmp_path / "idmap.json").source_ids(["d"]) == [4]


def test_an_integer_and_a_string_spelled_alike_are_two_ids(tmp_path):
    id_map = IdMap(tmp_path / "idmap.json")
    assert id_map.source_ids([1, "1"]) == [1, 2]


def test_a_frame_is_written_with_the_entity_id_column_the_build_reads(tmp_path):
    db = create(tmp_path / "db")
    staged = db.stage("points", frame(id=["p", "q", "r"]), id="id", default=True)
    table = pq.read_table(staged.path)
    assert table["entity_id"].to_pylist() == [1, 2, 3]
    assert table["entity_id"].type == pa.uint64()
    # The user's id column stays in the file, so the keyword attribute has a column to read.
    assert table["id"].to_pylist() == ["p", "q", "r"]
    assert staged.user_id_column == "id"


def test_a_named_index_is_the_id_column_under_its_own_name(tmp_path):
    db = create(tmp_path / "db")
    data = frame().set_index(pd.Index(["p", "q", "r"], name="paper"))
    staged = db.stage("points", data, default=True)
    assert staged.user_id_column == "paper"
    assert pq.read_table(staged.path)["paper"].to_pylist() == ["p", "q", "r"]


def test_a_default_index_is_the_row_position_at_the_first_commit_and_refused_on_a_delta(tmp_path):
    db = create(tmp_path / "db")
    staged = db.stage("points", frame(), default=True)
    assert staged.default_index
    assert "row positions" in " ".join(staged.notes)
    db.built = True
    with pytest.raises(Refusal, match="id="):
        db.stage("points", frame(), default=True)


def test_a_points_file_with_an_integer_entity_id_is_read_in_place(tmp_path):
    path = tmp_path / "points.parquet"
    pq.write_table(
        pa.table({"entity_id": pa.array([7, 8], type=pa.uint64()), "x": [1.0, 2.0]}), path
    )
    db = create(tmp_path / "db")
    staged = db.stage("points", str(path), default=True)
    assert staged.in_place and staged.path == path
    # The map is the identity over its ids, so no keyword attribute is written: the user's id is
    # the source id.
    assert staged.user_id_column is None
    assert len(db.id_map) == 0
    assert not staged.declared_path.startswith("/")


def test_a_members_file_beside_it_is_read_in_place_too(tmp_path):
    path = tmp_path / "members.parquet"
    pq.write_table(pa.table({"key": ["a"], "entity": pa.array([7], type=pa.uint64())}), path)
    db = create(tmp_path / "db")
    assert db.stage("members", str(path)).in_place


def test_a_file_whose_id_is_not_an_integer_is_mapped_and_written_under_sources(tmp_path):
    path = tmp_path / "points.parquet"
    pq.write_table(pa.table({"paper": ["p", "q"], "x": [1.0, 2.0]}), path)
    db = create(tmp_path / "db")
    staged = db.stage("points", str(path), id="paper", default=True)
    assert not staged.in_place
    assert staged.declared_path == "sources/points.parquet"
    assert "written under sources/" in " ".join(staged.notes)
    assert pq.read_table(staged.path)["entity_id"].to_pylist() == [1, 2]


def test_a_source_naming_no_entity_is_left_alone(tmp_path):
    path = tmp_path / "values.parquet"
    pq.write_table(pa.table({"key": ["low"], "code": pa.array([1], type=pa.uint32())}), path)
    db = create(tmp_path / "db")
    staged = db.stage("severity", str(path))
    assert staged.in_place and len(db.id_map) == 0


def test_staging_a_name_twice_replaces_the_earlier_data(tmp_path):
    db = create(tmp_path / "db")
    db.stage("points", frame(2, id=["p", "q"]), id="id", default=True)
    db.stage("points", frame(3, id=["p", "q", "r"]), id="id", default=True)
    assert db.sources["points"].rows == 3
    assert len(db.id_map) == 3


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


def test_open_refuses_a_directory_with_no_bundle(tmp_path):
    db = create(tmp_path / "db")
    db.stage("points", frame(id=["p", "q", "r"]), id="id", default=True)
    db.declare_view("map", source="points")
    db.write()
    from tesseradb._database import open as open_database

    with pytest.raises(Refusal, match="no built bundle"):
        open_database(tmp_path / "db")
    with pytest.raises(Refusal, match="no tessera.toml"):
        open_database(tmp_path)


def test_a_built_database_refuses_the_verbs_that_would_start_a_later_commit(tmp_path):
    db = Database(tmp_path)
    db.built = True
    for call in (
        lambda: db.declare_view("v", source="points"),
        lambda: db.declare_attribute("a", type="u8"),
        lambda: db.check(),
        lambda: db.commit(),
    ):
        with pytest.raises(Refusal, match="S3"):
            call()
