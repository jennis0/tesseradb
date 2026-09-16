"""§4.5's inference table, one row at a time, and the width an open vocabulary takes (§4.4)."""

import datetime as dt

import pyarrow as pa
import pytest

from tesseradb._database import create
from tesseradb._infer import FEW_DISTINCT, infer, width_above

pd = pytest.importorskip("pandas")


def inferred(**columns):
    rows, vocabularies, table = infer(pa.table(columns), set())
    return {row["name"]: row for row in rows}, vocabularies, {r.name: r for r in table}


def test_a_number_a_boolean_and_a_datetime_take_the_matching_width_rendered_and_indexed():
    blocks, _, rows = inferred(
        count=pa.array([1, 2], type=pa.int32()),
        score=pa.array([0.5, 1.5], type=pa.float32()),
        flag=pa.array([True, False]),
        seen=pa.array([dt.datetime(2026, 1, 1), dt.datetime(2026, 1, 2)], type=pa.timestamp("us")),
    )
    assert blocks["count"] == {"name": "count", "type": "i32", "render": True, "index": True}
    assert blocks["score"]["type"] == "f32"
    assert blocks["flag"]["type"] == "bool"
    assert blocks["seen"]["type"] == "timestamp_us"
    assert all(row.render and row.index for row in rows.values())


def test_a_string_of_few_values_is_a_category_over_an_open_public_vocabulary():
    blocks, vocabularies, rows = inferred(archive=pa.array(["cs", "math", "cs"]))
    assert blocks["archive"]["type"] == "category"
    assert blocks["archive"]["vocabulary"] == "archive"
    assert vocabularies == [
        {"name": "archive", "width": "u16", "value_set": "open", "visibility": "public"}
    ]
    assert "2 distinct" in rows["archive"].why


def test_a_string_of_many_short_values_is_a_keyword_and_a_long_one_is_text():
    ids = [f"identifier-{i}" for i in range(FEW_DISTINCT + 1)]
    prose = [f"a sentence of some length, number {i}, " + "x" * 64 for i in range(FEW_DISTINCT + 1)]
    blocks, _, rows = inferred(arxiv_id=pa.array(ids), abstract=pa.array(prose))
    assert blocks["arxiv_id"] == {"name": "arxiv_id", "type": "keyword", "index": True}
    # `render` is refused on a text column, the hot slot being fixed width.
    assert blocks["abstract"] == {"name": "abstract", "type": "text", "index": True}
    assert not rows["abstract"].render


def test_a_list_of_strings_and_an_unmapped_type_are_not_inferred():
    blocks, _, rows = inferred(
        categories=pa.array([["cs.LG"], ["math.AG"]]),
        blob=pa.array([b"a", b"b"]),
    )
    assert blocks == {}
    assert rows["categories"].declared_as == "not inferred"
    assert "access column" in rows["categories"].why
    assert rows["blob"].declared_as == "not inferred"


def test_a_vocabularys_width_is_one_above_what_its_values_need():
    assert width_above(2) == "u16"
    assert width_above(254) == "u16"
    assert width_above(1000) == "u32"
    assert width_above(10**6) == "u32"


def test_inference_reads_the_default_source_and_nothing_else(tmp_path):
    db = create(tmp_path / "db")
    frame = pd.DataFrame({"id": ["p", "q"], "x": [0.0, 1.0], "y": [0.0, 1.0], "year": [2025, 2026]})
    db.stage("points", frame, id="id", default=True)
    db.stage("other", pd.DataFrame({"id": ["p"], "sentiment": [0.5]}), id="id")
    db.declare_view("s0")
    text = db.declaration
    assert '[[attribute]]\nname = "year"\ntype = "i64"' in text
    # On a source that is not the default nothing is inferred.
    assert "sentiment" not in text


def test_the_view_and_a_layers_column_are_claimed_rather_than_inferred(tmp_path):
    db = create(tmp_path / "db")
    frame = pd.DataFrame(
        {
            "id": ["p", "q"],
            "x": [0.0, 1.0],
            "y": [0.0, 1.0],
            "terms": [["a"], ["b"]],
            "cluster": ["k0", "k1"],
        }
    )
    db.stage("points", frame, id="id", default=True)
    db.declare_view("s0", access="terms")
    db.declare_layer("clusters", kind="flat", from_column="cluster")
    text = db.declaration
    assert 'name = "x"' not in text and 'name = "terms"' not in text
    assert 'name = "cluster"' not in text


def test_the_report_states_the_thresholds_and_the_vocabularies_it_declared_open(tmp_path):
    db = create(tmp_path / "db")
    frame = pd.DataFrame({"id": ["p", "q"], "x": [0.0, 1.0], "y": [0.0, 1.0], "kind": ["a", "b"]})
    db.stage("points", frame, id="id", default=True)
    db.declare_view("s0")
    db.write()
    lines = "\n".join(db._inference.lines())
    assert "at most 4096 distinct values is a category" in lines
    assert "declared open and public" in lines and "kind" in lines
