"""How a row is named, from the frame to the served answer.

A table names each row's item by the columns it carries of the attributes the user declared
`unique`, at the build and on every later route. A row of the points that carries none is an item
of its own, addressable by the `tessera_id` the server hands back. The SDK keeps no map between
them: what is inserted is what is written, and what is sent is what the column holds.
"""

from __future__ import annotations

import pyarrow as pa
import pytest

from conftest import browse, item, viewport
from tesseradb._refusal import Refusal

pytest.importorskip("pyarrow")

FRAME = [-5.0, -5.0, 40.0, 40.0]


def papers(keys, x: float = 0.0) -> pa.Table:
    return pa.table(
        {
            "paper": pa.array(list(keys), pa.string()),
            "x": pa.array([x + i for i in range(len(keys))], pa.float64()),
            "y": pa.array([0.0] * len(keys), pa.float64()),
            "labels": pa.array([["public"]] * len(keys), pa.list_(pa.string())),
        }
    )


def string_ids(db) -> None:
    """A corpus named by a string column, with a clustering whose members name the same keys."""
    keys = [f"p{i}" for i in range(20)]
    db.declare_view("map", extent={"x": [-5, 40], "y": [-5, 40]})
    db.declare_attribute("paper", type="keyword", unique=True)
    db.declare_layer("clusters", kind="flat")
    db.insert("map", papers(keys), x="x", y="y", access="labels")
    db.insert(
        "clusters",
        artifacts=pa.table(
            {"level": pa.array([0], pa.uint32()), "key": pa.array(["c0"], pa.string())}
        ),
        key="key",
        level="level",
    )
    db.insert(
        "clusters",
        members=pa.table(
            {
                "level": pa.array([0] * len(keys), pa.uint32()),
                "key": pa.array(["c0"] * len(keys), pa.string()),
                "paper": pa.array(keys, pa.string()),
            }
        ),
        key="key",
        level="level",
    )


def test_a_string_id_column_names_the_rows_the_members_name_and_reaches_the_drill_down(
    served, corpus
):
    """The column is the unique attribute's value, and the members table names the same values."""
    db = served(string_ids)
    # Every table carries `paper` under its own name, so no block's `fields` moves it.
    assert 'paper = "' not in db.declaration

    answer = viewport(db, "map", FRAME)
    assert answer["counts"]["visible"] == 20
    assert browse(db, "map", "clusters")["artifacts"][0]["masked_count"] == 20

    record = item(db, answer["ids"][0])
    assert record["fields"]["paper"].startswith("p")


def test_a_delta_of_string_ids_is_ingested_and_a_second_page_of_them_edits_them(served, corpus):
    """A delta names its rows the same way, and a row naming an item the database holds edits it."""
    db = served(string_ids)
    fresh = [f"q{i}" for i in range(5)]
    db.insert("map", papers(fresh, x=25.0), x="x", y="y", access="labels")
    db.insert(
        "clusters",
        members=pa.table(
            {
                "level": pa.array([0] * len(fresh), pa.uint32()),
                "key": pa.array(["c0"] * len(fresh), pa.string()),
                "paper": pa.array(fresh, pa.string()),
            }
        ),
        key="key",
        level="level",
    )
    report = db.commit()
    assert report.ok, report
    assert report.rows_accepted == {"map": 5}
    assert report.flush_wait is not None and report.flush_reached
    assert viewport(db, "map", FRAME)["counts"]["visible"] == 25
    assert browse(db, "map", "clusters")["artifacts"][0]["masked_count"] == 25

    # The same keys again, moved a little: each row names its item and moves it.
    db.insert("map", papers(fresh, x=26.0), x="x", y="y", access="labels")
    again = db.commit()
    assert again.ok, again
    assert again.rows_accepted == {"map": 0} and again.items_edited == len(fresh), again
    assert viewport(db, "map", FRAME)["counts"]["visible"] == 25
    assert browse(db, "map", "clusters")["artifacts"][0]["masked_count"] == 25


def unnamed(db) -> None:
    """A frame carrying no unique column: each row is an item of its own."""
    import pandas as pd

    db.declare_view("map", extent={"x": [-5, 40], "y": [-5, 40]})
    db.insert(
        "map",
        pd.DataFrame(
            {
                "x": [float(i) for i in range(20)],
                "y": [0.0] * 20,
                "labels": [["public"]] * 20,
            }
        ),
        x="x",
        y="y",
        access="labels",
    )


def test_an_unnamed_index_is_the_tessera_id_route_and_remove_addresses_by_it(served, corpus):
    """No attribute is declared unique, and a row is addressed by the id the server hands back."""
    pytest.importorskip("pandas")
    db = served(unnamed)
    assert "unique" not in db.declaration

    answer = viewport(db, "map", FRAME)
    assert answer["counts"]["visible"] == 20
    picked = answer["ids"][0]

    report = db.remove([picked])
    assert report.ok, report
    assert viewport(db, "map", FRAME)["counts"]["visible"] == 19


def test_a_row_carrying_no_unique_column_creates_an_item_after_the_first_commit(served, corpus):
    """Points naming no item are items of their own, at a running server as at the build."""
    db = served(string_ids)
    frame = papers(["r0", "r1", "r2"], x=30.0).drop_columns(["paper"])
    db.insert("map", frame, x="x", y="y", access="labels")
    report = db.commit()
    assert report.ok, report
    assert report.rows_accepted == {"map": 3} and report.refused == []
    assert len(report.tessera_ids) == 3 and None not in report.tessera_ids
    assert viewport(db, "map", FRAME)["counts"]["visible"] == 23


def test_a_row_repeating_an_earlier_rows_unique_value_is_left_out_and_counted(served, corpus):
    """The first row of a repeated value is kept; the later one is refused, has no `tessera_id`,
    and the report counts it by its row in the table inserted."""
    db = served(string_ids)
    db.insert("map", papers(["r0", "r0", "r1"], x=30.0), x="x", y="y", access="labels")
    report = db.commit()
    assert report.ok, report
    assert report.rows_accepted == {"map": 2}
    assert [one["row"] for one in report.refused] == [1]
    assert sum(report.refused_by_reason.values()) == 1
    assert report.tessera_ids[1] is None and None not in (report.tessera_ids[0], report.tessera_ids[2])
    assert viewport(db, "map", FRAME)["counts"]["visible"] == 22


def test_remove_names_items_by_a_unique_column_or_by_a_list_of_tessera_ids(served, corpus):
    """A table's rows name items by the unique columns they carry; a bare list is `tessera_id`s."""
    db = served(string_ids)
    report = db.remove({"paper": ["p0", "p1"]})
    assert report.ok and report.accepted == 2 and report.refused == [], report
    assert viewport(db, "map", FRAME)["counts"]["visible"] == 18

    held = db.lookup("map", "paper", ["p2", "p3"]).column("tessera_id").to_pylist()
    report = db.remove(held)
    assert report.ok and report.accepted == 2, report
    assert viewport(db, "map", FRAME)["counts"]["visible"] == 16

    # A table read back from the database names each item twice over, by both of its columns.
    report = db.remove(db.lookup("map", "paper", ["p4"]))
    assert report.accepted == 1, report
    assert viewport(db, "map", FRAME)["counts"]["visible"] == 15


def test_a_remove_naming_nothing_is_refused_by_row_and_strict_refuses_the_whole_call(
    served, corpus
):
    """A row naming no item is listed with its reason and the others are applied; with `strict`
    the call applies nothing and raises."""
    db = served(string_ids)
    report = db.remove({"paper": ["nobody", "p5"]})
    assert report.ok and report.accepted == 1, report
    assert report.refused == [{"row": 0, "reason": "names_no_item"}]
    assert report.refused_by_reason == {"names_no_item": 1}
    assert viewport(db, "map", FRAME)["counts"]["visible"] == 19

    with pytest.raises(Refusal) as raised:
        db.remove({"paper": ["p6", "nobody"]}, strict=True)
    assert raised.value.report.accepted == 0 and [
        one["status"] for one in raised.value.report.refusals
    ] == [404]
    assert viewport(db, "map", FRAME)["counts"]["visible"] == 19

    # A column that is neither `tessera_id` nor a unique attribute: the server refuses the call.
    with pytest.raises(Refusal):
        db.remove({"title": ["p6"]})
    assert viewport(db, "map", FRAME)["counts"]["visible"] == 19


def test_members_are_named_by_any_mix_of_tessera_id_and_unique_columns(served, corpus):
    """A members table carrying `tessera_id` and a unique column names each member by whichever
    its row carries; a member naming nothing is left out and counted."""
    db = served(string_ids)
    [by_id] = db.lookup("map", "paper", ["p1"]).column("tessera_id").to_pylist()
    db.insert(
        "clusters",
        artifacts=pa.table({"level": pa.array([0], pa.uint32()), "key": pa.array(["c1"])}),
        key="key",
        level="level",
    )
    db.insert(
        "clusters",
        members=pa.table(
            {
                "level": pa.array([0, 0, 0], pa.uint32()),
                "key": pa.array(["c1", "c1", "c1"]),
                "tessera_id": pa.array([str(by_id), None, None]),
                "paper": pa.array([None, "p2", "nobody"]),
            }
        ),
        key="key",
        level="level",
    )
    report = db.commit()
    assert report.ok, report
    assert [(one["key"], one["member"], one["reason"]) for one in report.refused] == [
        ("c1", {"paper": "nobody"}, "names_no_item")
    ]
    counts = {row["key"]: row["masked_count"] for row in browse(db, "map", "clusters")["artifacts"]}
    assert counts == {"c0": 20, "c1": 2}

    # Strict, the same member refuses its publication.
    db.insert(
        "clusters",
        members=pa.table({"key": pa.array(["c1"]), "paper": pa.array(["nobody"])}),
        key="key",
    )
    with pytest.raises(Refusal):
        db.commit(strict=True)
