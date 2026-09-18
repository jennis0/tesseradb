"""`declare_columns`: the frame's dtypes, declared as details, and nothing inferred (§4.5)."""

import pyarrow as pa
import pytest

from tesseradb._database import create

pd = pytest.importorskip("pandas")


def frame():
    return pa.table(
        {
            "paper": pa.array(["p", "q"], pa.string()),
            "x": pa.array([0.0, 1.0], pa.float64()),
            "y": pa.array([0.0, 1.0], pa.float64()),
            "year": pa.array([1999, 2003], pa.int64()),
            "open": pa.array([True, False], pa.bool_()),
            "submitted": pa.array([0, 1], pa.timestamp("us")),
            "title": pa.array(["one", "two"], pa.string()),
            "archive": pa.array(["cs", "math"], pa.string()),
            "arxiv_id": pa.array(["2601.1", "2601.2"], pa.string()),
            "terms": pa.array([["a"], ["b"]], pa.list_(pa.string())),
        }
    )


@pytest.fixture
def db(tmp_path):
    database = create(tmp_path / "db")
    database.declare_view("map")
    return database


def test_every_column_is_declared_from_its_dtype_as_details(db):
    report = db.declare_columns(frame(), skip=["paper", "x", "y", "terms"])
    declared = {row.name: row for row in report.columns}
    assert declared["year"].declared_as == "i64"
    assert declared["open"].declared_as == "bool"
    assert declared["submitted"].declared_as == "timestamp_us"
    assert declared["title"].declared_as == "text"
    # Details only: neither rendered nor indexed unless the call named the column.
    assert not any(row.render or row.index for row in report.columns)
    text = db.declaration
    assert 'name = "year"\ntype = "i64"' in text
    assert "render" not in text and "index" not in text


def test_render_and_index_apply_to_the_columns_named(db):
    report = db.declare_columns(
        frame(), skip=["paper", "x", "y", "terms"], render=["year"], index=["year", "title"]
    )
    declared = {row.name: row for row in report.columns}
    assert declared["year"].render and declared["year"].index
    assert declared["title"].index and not declared["title"].render
    text = db.declaration
    assert 'name = "year"\ntype = "i64"\nrender = true\nindex = true' in text


def test_a_string_is_text_unless_the_call_names_it_keyword_or_category(db):
    report = db.declare_columns(
        frame(),
        skip=["paper", "x", "y", "terms"],
        keyword=["arxiv_id"],
        category=["archive"],
    )
    declared = {row.name: row for row in report.columns}
    assert declared["title"].declared_as == "text"
    assert declared["arxiv_id"].declared_as == "keyword"
    assert declared["archive"].declared_as == "category"
    assert report.vocabularies == ["archive"]
    text = db.declaration
    assert '[[vocabulary]]\nname = "archive"\nwidth = "u16"\nvalue_set = "open"' in text
    assert 'vocabulary = "archive"' in text


def test_a_list_and_a_type_with_no_mapping_are_not_declared_and_are_listed(db):
    report = db.declare_columns(frame(), skip=["paper", "x", "y"])
    declared = {row.name: row for row in report.columns}
    assert declared["terms"].declared_as == "not declared"
    assert "access=" in declared["terms"].why
    assert 'name = "terms"' not in db.declaration


def test_a_column_already_declared_is_left_as_it_was(db):
    db.declare_attribute("year", type="i32", index=True)
    report = db.declare_columns(frame(), skip=["paper", "x", "y", "terms"])
    assert "year" not in {row.name for row in report.columns}
    assert db.declaration.count('name = "year"') == 1
    assert 'type = "i32"' in db.declaration


def test_the_helper_reads_the_schema_and_prints_the_table_it_declared(db, capsys):
    db.declare_columns(frame(), skip=["paper", "x", "y", "terms"])
    printed = capsys.readouterr().out
    assert "declare_columns" in printed
    assert "declared as" in printed and "timestamp_us" in printed


def test_the_columns_a_frame_fills_are_matched_on_the_allocation_views_insert(db):
    db.declare_columns(frame(), skip=["paper", "x", "y", "terms"])
    insert = db.insert("map", frame(), id="paper", x="x", y="y", access="terms")
    assert set(insert.named_attributes) == {
        "year", "open", "submitted", "title", "archive", "arxiv_id"
    }
    text = db.declaration
    assert text.count('source = "map"') == 1 + 6  # the view, and the six columns it fills
    assert 'entity_id_field = "paper"' in text


def test_a_second_views_insert_fills_nothing_by_name(db):
    db.declare_view("second")
    db.declare_columns(frame(), skip=["paper", "x", "y", "terms"])
    insert = db.insert("second", frame(), id="paper", x="x", y="y", access="terms")
    assert insert.named_attributes == {}
    assert insert.ignored == ["year", "open", "submitted", "title", "archive", "arxiv_id"]


def test_columns_names_one_attributes_value_column_explicitly(db):
    db.declare_attribute("year", type="i64", index=True)
    insert = db.insert(
        "map", frame(), id="paper", x="x", y="y", columns={"year": "year"}
    )
    assert insert.named_attributes == {"year": "year"}
